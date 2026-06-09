/*
Copyright 2023 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::error::Error;

use anyhow::{anyhow, Ok, Result};
use cgroups_rs::{
    cgroup_builder::CgroupBuilder, cpu::CpuController, cpuset::CpuSetController,
    hugetlb::HugeTlbController, memory::MemController, Cgroup,
};
use containerd_sandbox::{cri::api::v1::LinuxContainerResources, data::SandboxData};
use serde::{Deserialize, Serialize};

use crate::{
    utils::{get_overhead_resources, get_resources, get_total_resources},
    vm::VcpuThreads,
};

pub const DEFAULT_CGROUP_PARENT_PATH: &str = "kuasar-vmm";
pub const VCPU_CGROUP_NAME: &str = "vcpu";
pub const POD_OVERHEAD_CGROUP_NAME: &str = "pod_overhead";

/// `SandboxCgroup` represents a set of cgroups for a sandbox.
#[derive(Default, Debug, Serialize, Deserialize)]
pub struct SandboxCgroup {
    pub cgroup_parent_path: String,
    #[serde(skip)]
    pub sandbox_cgroup: Option<Cgroup>,
    #[serde(skip)]
    pub vcpu_cgroup: Option<Cgroup>,
    #[serde(skip)]
    pub pod_overhead_cgroup: Option<Cgroup>,
}

impl SandboxCgroup {
    pub fn create_sandbox_cgroups(cgroup_parent_path: &str, sandbox_id: &str) -> Result<Self> {
        // Create sandbox cgroup in the all cgroup subsystem dir
        let sandbox_cgroup_path = format!("{}/{}", cgroup_parent_path, sandbox_id);
        // CgroupBuilder::new() func doesn't accept the cgroup name has "/" prefix,
        // So need to remove the "/" prefix for sandbox_cgroup_path
        let sandbox_cgroup_rela_path = sandbox_cgroup_path.trim_start_matches('/');

        let sandbox_cgroup = CgroupBuilder::new(sandbox_cgroup_rela_path)
            .set_specified_controllers(vec!["cpu".to_string(), "memory".to_string()])
            .build(cgroups_rs::hierarchies::auto())?;

        // Only create the vcpu and pod_overhead cgroups in the cpu cgroup subsystem
        let vcpu_cgroup_path = format!("{}/{}", sandbox_cgroup_rela_path, VCPU_CGROUP_NAME);
        let vcpu_cgroup = CgroupBuilder::new(vcpu_cgroup_path.as_str())
            .set_specified_controllers(vec!["cpu".to_string()])
            .build(cgroups_rs::hierarchies::auto())?;

        let pod_overhead_cgroup_path =
            format!("{}/{}", sandbox_cgroup_rela_path, POD_OVERHEAD_CGROUP_NAME);
        let pod_overhead_cgroup = CgroupBuilder::new(pod_overhead_cgroup_path.as_str())
            .set_specified_controllers(vec!["cpu".to_string()])
            .build(cgroups_rs::hierarchies::auto())?;

        Ok(SandboxCgroup {
            cgroup_parent_path: cgroup_parent_path.to_string(),
            sandbox_cgroup: Some(sandbox_cgroup),
            vcpu_cgroup: Some(vcpu_cgroup),
            pod_overhead_cgroup: Some(pod_overhead_cgroup),
        })
    }

    pub fn update_res_for_sandbox_cgroups(&self, sandbox_data: &SandboxData) -> Result<()> {
        if let Some(ref sandbox_cgroup) = self.sandbox_cgroup {
            // apply the total resources = sum(sum(containers_resources) + pod_overhead)) in the sandbox cgroup dir
            if let Some(total_resources) = get_total_resources(sandbox_data) {
                apply_cpu_resource(sandbox_cgroup, &total_resources)?;
                apply_memory_resource(sandbox_cgroup, &total_resources)?;
                apply_cpuset_resources(sandbox_cgroup, &total_resources)?;
                apply_hugetlb_resources(sandbox_cgroup, &total_resources)?;
            }
        }

        if let Some(ref vcpu_cgroup) = self.vcpu_cgroup {
            // apply the cpu resource of containers in the vcpu cpu subsystem cgroup
            if let Some(containers_resources) = get_resources(sandbox_data) {
                apply_cpu_resource(vcpu_cgroup, containers_resources)?;
            }
        }

        if let Some(ref pod_overhead_cgroup) = self.pod_overhead_cgroup {
            // apply the cpu resource of pod_overhead in the pod_overhead cpu subsystem cgroup
            if let Some(overhead_resources) = get_overhead_resources(sandbox_data) {
                apply_cpu_resource(pod_overhead_cgroup, overhead_resources)?;
            }
        }

        Ok(())
    }

    pub fn add_process_into_sandbox_cgroups(
        &self,
        pid: u32,
        vcpu_threads: Option<VcpuThreads>,
    ) -> Result<()> {
        // Add vmm process into the sandbox_cgroup
        if let Some(ref sandbox_cgroup) = self.sandbox_cgroup {
            sandbox_cgroup.add_task_by_tgid((pid as u64).into())?;
        }
        if let Some(ref pod_overhead_cgroup) = self.pod_overhead_cgroup {
            pod_overhead_cgroup.add_task_by_tgid((pid as u64).into())?;
        }

        if let Some(all_vcpu_threads) = vcpu_threads {
            if let Some(ref vcpu_cgroup) = self.vcpu_cgroup {
                // Move vmm process from parent sandbox cgroup into pod_overhead cgroup
                // Then move the all vcpu threads of vmm process into vcpu cgroup
                for (_, vcpu_thread_tid) in all_vcpu_threads.vcpus {
                    vcpu_cgroup.add_task((vcpu_thread_tid as u64).into())?;
                }
            }
        }

        Ok(())
    }

    pub fn remove_sandbox_cgroups(&self) -> Result<()> {
        if let Some(ref vcpu_cgroup) = self.vcpu_cgroup {
            remove_sandbox_cgroup(vcpu_cgroup)?;
        }
        if let Some(ref pod_overhead_cgroup) = self.pod_overhead_cgroup {
            remove_sandbox_cgroup(pod_overhead_cgroup)?;
        }
        if let Some(ref sandbox_cgroup) = self.sandbox_cgroup {
            remove_sandbox_cgroup(sandbox_cgroup)?;
        }
        Ok(())
    }
}

fn apply_cpu_resource(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let cpu_controller: &CpuController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No cpu controller attached!"))?;

    if res.cpu_period != 0 {
        cpu_controller.set_cfs_period(res.cpu_period.try_into()?)?;
    }
    if res.cpu_quota != 0 {
        cpu_controller.set_cfs_quota(res.cpu_quota)?;
    }
    if res.cpu_shares != 0 {
        cpu_controller.set_shares(res.cpu_shares.try_into()?)?;
    }

    Ok(())
}

fn apply_memory_resource(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    let mem_controller: &MemController = cgroup
        .controller_of()
        .ok_or_else(|| anyhow!("No memory controller attached!"))?;

    if res.memory_limit_in_bytes != 0 {
        mem_controller.set_limit(res.memory_limit_in_bytes)?;
    }
    if res.memory_swap_limit_in_bytes != 0 {
        mem_controller.set_memswap_limit(res.memory_swap_limit_in_bytes)?;
    }

    Ok(())
}

fn apply_cpuset_resources(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    if res.cpuset_cpus.is_empty() && res.cpuset_mems.is_empty() {
        return Ok(());
    }

    let cpuset_controller: &CpuSetController = match cgroup.controller_of() {
        Some(c) => c,
        None => {
            log::warn!("cpuset resource specified but no cpuset controller attached!");
            return Ok(());
        }
    };

    if !res.cpuset_cpus.is_empty() {
        cpuset_controller.set_cpus(&res.cpuset_cpus)?;
    }
    if !res.cpuset_mems.is_empty() {
        cpuset_controller.set_mems(&res.cpuset_mems)?;
    }

    Ok(())
}

fn apply_hugetlb_resources(cgroup: &Cgroup, res: &LinuxContainerResources) -> Result<()> {
    if res.hugepage_limits.is_empty() {
        return Ok(());
    }

    let hugetlb_controller: &HugeTlbController = match cgroup.controller_of() {
        Some(c) => c,
        None => {
            log::warn!("hugetlb resource specified but no hugetlb controller attached!");
            return Ok(());
        }
    };

    for h in res.hugepage_limits.iter() {
        hugetlb_controller.set_limit_in_bytes(h.page_size.as_str(), h.limit)?;
    }
    Ok(())
}

fn remove_sandbox_cgroup(cgroup: &Cgroup) -> Result<()> {
    // get the tids in the current cgroup and then move the tids to parent cgroup
    let tids = cgroup.tasks();
    for tid in tids {
        cgroup.move_task_to_parent(tid).unwrap_or_default();
    }

    // Should ignore the NotFound error of cgroup path as it may be already deleted.
    if let Err(e) = cgroup.delete() {
        if e.kind() == &cgroups_rs::error::ErrorKind::RemoveFailed {
            if let Some(cause) = e.source() {
                if let Some(ioe) = cause.downcast_ref::<std::io::Error>() {
                    if ioe.kind() == std::io::ErrorKind::NotFound {
                        return Ok(());
                    }
                }
            }
        }

        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, result::Result::Ok};

    use cgroups_rs::Controller;
    use containerd_sandbox::{
        cri::api::v1::{HugepageLimit, LinuxPodSandboxConfig},
        PodSandboxConfig,
    };

    use super::*;
    use crate::utils::get_sandbox_cgroup_parent_path;

    macro_rules! unwrap_cgroup {
        ($cgroups:expr, $field:ident) => {
            $cgroups.$field.as_ref().unwrap()
        };
    }

    fn create_mock_pod_sandbox_config() -> PodSandboxConfig {
        let mut pod_sandbox_config = PodSandboxConfig::default();
        pod_sandbox_config.linux = Some(LinuxPodSandboxConfig {
            cgroup_parent: "/kubepods/burstable/podxxx".to_string(),
            security_context: None,
            sysctls: HashMap::new(),
            overhead: Some(LinuxContainerResources {
                cpu_period: 100000,
                cpu_quota: 50000,
                cpu_shares: 1024,
                memory_limit_in_bytes: 100 * 1024 * 1024,
                oom_score_adj: 0,
                cpuset_cpus: "".to_string(),
                cpuset_mems: "".to_string(),
                hugepage_limits: vec![],
                unified: HashMap::new(),
                memory_swap_limit_in_bytes: 0 + 100 * 1024 * 1024,
            }),
            resources: Some(LinuxContainerResources {
                cpu_period: 100000,
                cpu_quota: 200000,
                cpu_shares: 1024,
                memory_limit_in_bytes: 1024 * 1024 * 1024,
                oom_score_adj: 0,
                cpuset_cpus: "0-1".to_string(),
                cpuset_mems: "0".to_string(),
                hugepage_limits: vec![HugepageLimit {
                    page_size: "2MB".to_string(),
                    limit: 2 * 1024 * 1024 * 1024,
                }],
                unified: HashMap::new(),
                memory_swap_limit_in_bytes: 0 + 1024 * 1024 * 1024,
            }),
        });
        pod_sandbox_config
    }

    #[test]
    fn test_create_sandbox_cgroups() {
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            return;
        }

        // Create a SandboxData instance
        let mut sandbox_data = SandboxData::default();
        sandbox_data.id = String::from("test_sandbox");

        // Case 1: sandbox.config is None, expect return the error
        sandbox_data.config = None;
        let sandbox_cgroup_path = get_sandbox_cgroup_parent_path(&sandbox_data);
        assert_eq!(sandbox_cgroup_path.is_none(), true);

        // Case 2: sandbox.config is corrent, expect create sandbox cgroup successfully
        let pod_sandbox_config = create_mock_pod_sandbox_config();
        sandbox_data.config = Some(pod_sandbox_config);
        let sandbox_cgroup_path = get_sandbox_cgroup_parent_path(&sandbox_data).unwrap();
        let result = SandboxCgroup::create_sandbox_cgroups(&sandbox_cgroup_path, &sandbox_data.id);
        match result {
            Ok(sandbox_cgoups) => {
                // Get the test environment cpu subsystem cgroup mountpoint path
                let cpu_cgroup_root_pathbuf = cgroups_rs::hierarchies::V1::new()
                    .get_mount_point(cgroups_rs::Controllers::Cpu)
                    .unwrap();
                println!("cpu_cgroup_root_pathbuf: {:?}", cpu_cgroup_root_pathbuf);

                // check sandbox level cgroup
                let sandbox_cgroup_cpu_controller: &CpuController =
                    unwrap_cgroup!(sandbox_cgoups, sandbox_cgroup)
                        .controller_of()
                        .unwrap();
                assert!(sandbox_cgroup_cpu_controller.path().exists());
                assert_eq!(
                    sandbox_cgroup_cpu_controller.path(),
                    cpu_cgroup_root_pathbuf
                        .join("kubepods/burstable/podxxx/test_sandbox")
                        .as_path()
                );

                // check vcpu level cgroup
                let vcpu_cgroup_cpu_controller: &CpuController =
                    unwrap_cgroup!(sandbox_cgoups, vcpu_cgroup)
                        .controller_of()
                        .unwrap();
                assert!(vcpu_cgroup_cpu_controller.path().exists());
                assert_eq!(
                    vcpu_cgroup_cpu_controller.path(),
                    cpu_cgroup_root_pathbuf
                        .join("kubepods/burstable/podxxx/test_sandbox/vcpu")
                        .as_path()
                );
            }
            Err(e) => panic!("Expected an Ok, but got error: {}", e.to_string()),
        }

        // Case 3: If sandbox cgroups already exist in the system, then call create_sandbox_cgroups
        //         function again will not fail
        let cgroups =
            SandboxCgroup::create_sandbox_cgroups("/kubepods/burstable/podxxx", "sandbox_id")
                .unwrap();
        let cpu_cgroup_root_pathbuf = cgroups_rs::hierarchies::V1::new()
            .get_mount_point(cgroups_rs::Controllers::Cpu)
            .unwrap();
        let sandbox_cgroup_cpu_controller: &CpuController = unwrap_cgroup!(cgroups, sandbox_cgroup)
            .controller_of()
            .unwrap();
        assert_eq!(
            sandbox_cgroup_cpu_controller.path(),
            cpu_cgroup_root_pathbuf
                .join("kubepods/burstable/podxxx/sandbox_id")
                .as_path()
        );
        let vcpu_cgroup_cpu_controller: &CpuController = unwrap_cgroup!(cgroups, vcpu_cgroup)
            .controller_of()
            .unwrap();
        assert_eq!(
            vcpu_cgroup_cpu_controller.path(),
            cpu_cgroup_root_pathbuf
                .join("kubepods/burstable/podxxx/sandbox_id/vcpu")
                .as_path()
        );
        let pod_overhead_cgroup_cpu_controller: &CpuController =
            unwrap_cgroup!(cgroups, pod_overhead_cgroup)
                .controller_of()
                .unwrap();
        assert_eq!(
            pod_overhead_cgroup_cpu_controller.path(),
            cpu_cgroup_root_pathbuf
                .join("kubepods/burstable/podxxx/sandbox_id/pod_overhead")
                .as_path()
        );

        // Clean the test sandbox cgroups
        assert_eq!(cgroups.remove_sandbox_cgroups().is_ok(), true);
    }

    #[test]
    fn test_update_res_for_sandbox_cgroups_success() {
        // Currently only support cgroup V1, cgroup V2 is not supported now
        if cgroups_rs::hierarchies::is_cgroup2_unified_mode() {
            return;
        }

        // Case 1: successfully case
        // Create a SandboxData instance
        let mut sandbox_data = SandboxData::default();
        sandbox_data.id = String::from("test_sandbox");
        sandbox_data.config = Some(create_mock_pod_sandbox_config());

        // Create a SandboxData instance
        let sandbox_cgroup_path = get_sandbox_cgroup_parent_path(&sandbox_data).unwrap();
        let sandbox_cgroups =
            SandboxCgroup::create_sandbox_cgroups(&sandbox_cgroup_path, &sandbox_data.id).unwrap();
        let result = sandbox_cgroups.update_res_for_sandbox_cgroups(&sandbox_data);

        match result {
            Ok(_) => {
                // Check sandbox level cgroup total resources
                // Check cpu subsystem cgroup limit
                let sandbox_cgroup_cpu_controller: &CpuController =
                    unwrap_cgroup!(sandbox_cgroups, sandbox_cgroup)
                        .controller_of()
                        .unwrap();
                assert_eq!(sandbox_cgroup_cpu_controller.cfs_period().unwrap(), 100000);
                assert_eq!(sandbox_cgroup_cpu_controller.cfs_quota().unwrap(), 250000);
                assert_eq!(sandbox_cgroup_cpu_controller.shares().unwrap(), 2048);

                // Check memory subsystem cgroup limit
                let sandbox_cgroup_mem_controller: &MemController =
                    unwrap_cgroup!(sandbox_cgroups, sandbox_cgroup)
                        .controller_of()
                        .unwrap();
                let memory_stats = sandbox_cgroup_mem_controller.memory_stat();
                let memory_swap_stats = sandbox_cgroup_mem_controller.memswap();
                assert_eq!(memory_stats.limit_in_bytes, 1124 * 1024 * 1024);
                assert_eq!(memory_swap_stats.limit_in_bytes, 1124 * 1024 * 1024);

                // check vcpu level cgroup
                let vcpu_cgroup_cpu_controller: &CpuController =
                    unwrap_cgroup!(sandbox_cgroups, vcpu_cgroup)
                        .controller_of()
                        .unwrap();
                assert_eq!(vcpu_cgroup_cpu_controller.cfs_period().unwrap(), 100000);
                assert_eq!(vcpu_cgroup_cpu_controller.cfs_quota().unwrap(), 200000);
                assert_eq!(vcpu_cgroup_cpu_controller.shares().unwrap(), 1024);

                // check pod_overhead level cgroup
                let pod_overhead_cgroup_cpu_controller: &CpuController =
                    unwrap_cgroup!(sandbox_cgroups, pod_overhead_cgroup)
                        .controller_of()
                        .unwrap();
                assert_eq!(
                    pod_overhead_cgroup_cpu_controller.cfs_period().unwrap(),
                    100000
                );
                assert_eq!(
                    pod_overhead_cgroup_cpu_controller.cfs_quota().unwrap(),
                    50000
                );
                assert_eq!(pod_overhead_cgroup_cpu_controller.shares().unwrap(), 1024);
            }
            Err(e) => panic!("Expected an Ok, but got error: {}", e.to_string()),
        }

        assert_eq!(sandbox_cgroups.remove_sandbox_cgroups().is_ok(), true);
    }

    #[test]
    fn test_empty_sandbox_cgroups_are_noop() {
        let mut sandbox_data = SandboxData::default();
        sandbox_data.id = String::from("test_sandbox");
        sandbox_data.config = Some(create_mock_pod_sandbox_config());

        let sandbox_cgroups = SandboxCgroup::default();

        assert!(sandbox_cgroups
            .update_res_for_sandbox_cgroups(&sandbox_data)
            .is_ok());
        assert!(sandbox_cgroups
            .add_process_into_sandbox_cgroups(
                1,
                Some(VcpuThreads {
                    vcpus: HashMap::from([(0, 1)]),
                }),
            )
            .is_ok());
        assert!(sandbox_cgroups.remove_sandbox_cgroups().is_ok());
    }
}
