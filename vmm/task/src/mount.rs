// Copyright (c) 2019 Ant Financial
//
// SPDX-License-Identifier: Apache-2.0
//

use std::collections::HashMap;

use containerd_shim::{io_error, Error};
use lazy_static::lazy_static;
use log::warn;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
};

use crate::StaticMount;

pub const SYSFS_CGROUPPATH: &str = "/sys/fs/cgroup";
#[allow(dead_code)]
pub const SYSFS_ONLINE_FILE: &str = "online";

#[allow(dead_code)]
pub const PROC_MOUNTSTATS: &str = "/proc/self/mountstats";
pub const PROC_CGROUPS: &str = "/proc/cgroups";

lazy_static! {
    static ref CGROUPS: HashMap<&'static str, &'static str> = {
        let mut m = HashMap::new();
        m.insert("cpu", "/sys/fs/cgroup/cpu");
        m.insert("cpuacct", "/sys/fs/cgroup/cpuacct");
        m.insert("blkio", "/sys/fs/cgroup/blkio");
        m.insert("cpuset", "/sys/fs/cgroup/cpuset");
        m.insert("memory", "/sys/fs/cgroup/memory");
        m.insert("devices", "/sys/fs/cgroup/devices");
        m.insert("freezer", "/sys/fs/cgroup/freezer");
        m.insert("net_cls", "/sys/fs/cgroup/net_cls");
        m.insert("perf_event", "/sys/fs/cgroup/perf_event");
        m.insert("net_prio", "/sys/fs/cgroup/net_prio");
        m.insert("hugetlb", "/sys/fs/cgroup/hugetlb");
        m.insert("pids", "/sys/fs/cgroup/pids");
        m.insert("rdma", "/sys/fs/cgroup/rdma");
        m
    };
}

pub async fn get_cgroup_mounts(
    cg_path: &str,
    unified_cgroup_hierarchy: bool,
) -> containerd_shim::Result<Vec<StaticMount>> {
    // cgroup v2
    if unified_cgroup_hierarchy {
        return Ok(vec![StaticMount {
            fstype: "cgroup2",
            src: "cgroup2",
            dest: "/sys/fs/cgroup",
            options: vec!["nosuid", "nodev", "noexec", "relatime", "nsdelegate"],
        }]);
    }

    let file = File::open(&cg_path)
        .await
        .map_err(io_error!(e, "failed to open {}: ", cg_path))?;
    let reader = BufReader::new(file);

    let mut has_device_cgroup = false;
    let mut cg_mounts: Vec<StaticMount> = vec![StaticMount {
        fstype: "tmpfs",
        src: "tmpfs",
        dest: SYSFS_CGROUPPATH,
        options: vec!["nosuid", "nodev", "noexec", "mode=755"],
    }];

    // #subsys_name    hierarchy       num_cgroups     enabled
    // fields[0]       fields[1]       fields[2]       fields[3]
    let mut lines = reader.lines();
    'outer: while let Some(line) =
        lines
            .next_line()
            .await
            .map_err(io_error!(e, "failed to read {}: ", cg_path))?
    {
        let fields: Vec<&str> = line.split('\t').collect();
        // Ignore comment header
        if fields[0].starts_with('#') {
            continue;
        }
        // Ignore truncated lines
        if fields.len() < 4 {
            continue;
        }
        // Ignore disabled cgroups
        if fields[3] == "0" {
            continue;
        }
        // Ignore fields containing invalid numerics
        for f in [fields[1], fields[2], fields[3]].iter() {
            if f.parse::<u64>().is_err() {
                continue 'outer;
            }
        }
        let subsystem_name = fields[0];
        if subsystem_name.is_empty() {
            continue;
        }
        if subsystem_name == "devices" {
            has_device_cgroup = true;
        }

        if let Some((key, value)) = CGROUPS.get_key_value(subsystem_name) {
            cg_mounts.push(StaticMount {
                fstype: "cgroup",
                src: "cgroup",
                dest: value,
                options: vec!["nosuid", "nodev", "noexec", "relatime", key],
            });
        }
    }

    if !has_device_cgroup {
        warn!("The system didn't support device cgroup, which is dangerous, thus agent initialized without cgroup support!\n");
        return Ok(Vec::new());
    }

    cg_mounts.push(StaticMount {
        fstype: "tmpfs",
        src: "tmpfs",
        dest: SYSFS_CGROUPPATH,
        options: vec!["remount", "ro", "nosuid", "nodev", "noexec", "mode=755"],
    });

    Ok(cg_mounts)
}
