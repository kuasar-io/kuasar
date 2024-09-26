/*
   Copyright The containerd Authors.

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

use std::{
    collections::HashMap,
    fs,
    ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not},
    path::Path,
};

use anyhow::{anyhow, Result};
use fs::read_to_string;
use lazy_static::lazy_static;
use log::debug;
use nix::{errno::Errno, libc, mount::MsFlags, NixPath};
use regex::Regex;

pub const MNT_NOFOLLOW: i32 = 0x8;
pub const MNT_OPTION_MAX_LEN: usize = 4096;

struct Flag {
    clear: bool,
    flags: MsFlags,
}

lazy_static! {
    static ref MOUNT_FLAGS: HashMap<&'static str, Flag> = {
        let mut mf = HashMap::new();
        let zero: MsFlags = MsFlags::from_bits(0).unwrap();
        mf.insert(
            "async",
            Flag {
                clear: true,
                flags: MsFlags::MS_SYNCHRONOUS,
            },
        );
        mf.insert(
            "atime",
            Flag {
                clear: true,
                flags: MsFlags::MS_NOATIME,
            },
        );
        mf.insert(
            "bind",
            Flag {
                clear: false,
                flags: MsFlags::MS_BIND,
            },
        );
        mf.insert(
            "defaults",
            Flag {
                clear: false,
                flags: zero,
            },
        );
        mf.insert(
            "dev",
            Flag {
                clear: true,
                flags: MsFlags::MS_NODEV,
            },
        );
        mf.insert(
            "diratime",
            Flag {
                clear: true,
                flags: MsFlags::MS_NODIRATIME,
            },
        );
        mf.insert(
            "dirsync",
            Flag {
                clear: false,
                flags: MsFlags::MS_DIRSYNC,
            },
        );
        mf.insert(
            "exec",
            Flag {
                clear: true,
                flags: MsFlags::MS_NOEXEC,
            },
        );
        mf.insert(
            "mand",
            Flag {
                clear: false,
                flags: MsFlags::MS_MANDLOCK,
            },
        );
        mf.insert(
            "noatime",
            Flag {
                clear: false,
                flags: MsFlags::MS_NOATIME,
            },
        );
        mf.insert(
            "nodev",
            Flag {
                clear: false,
                flags: MsFlags::MS_NODEV,
            },
        );
        mf.insert(
            "nodiratime",
            Flag {
                clear: false,
                flags: MsFlags::MS_NODIRATIME,
            },
        );
        mf.insert(
            "noexec",
            Flag {
                clear: false,
                flags: MsFlags::MS_NOEXEC,
            },
        );
        mf.insert(
            "nomand",
            Flag {
                clear: true,
                flags: MsFlags::MS_MANDLOCK,
            },
        );
        mf.insert(
            "norelatime",
            Flag {
                clear: true,
                flags: MsFlags::MS_RELATIME,
            },
        );
        mf.insert(
            "nostrictatime",
            Flag {
                clear: true,
                flags: MsFlags::MS_STRICTATIME,
            },
        );
        mf.insert(
            "nosuid",
            Flag {
                clear: false,
                flags: MsFlags::MS_NOSUID,
            },
        );
        mf.insert(
            "rbind",
            Flag {
                clear: false,
                flags: MsFlags::MS_BIND | MsFlags::MS_REC,
            },
        );
        mf.insert(
            "relatime",
            Flag {
                clear: false,
                flags: MsFlags::MS_RELATIME,
            },
        );
        mf.insert(
            "remount",
            Flag {
                clear: false,
                flags: MsFlags::MS_REMOUNT,
            },
        );
        mf.insert(
            "ro",
            Flag {
                clear: false,
                flags: MsFlags::MS_RDONLY,
            },
        );
        mf.insert(
            "rw",
            Flag {
                clear: true,
                flags: MsFlags::MS_RDONLY,
            },
        );
        mf.insert(
            "strictatime",
            Flag {
                clear: false,
                flags: MsFlags::MS_STRICTATIME,
            },
        );
        mf.insert(
            "suid",
            Flag {
                clear: true,
                flags: MsFlags::MS_NOSUID,
            },
        );
        mf.insert(
            "sync",
            Flag {
                clear: false,
                flags: MsFlags::MS_SYNCHRONOUS,
            },
        );
        mf
    };
    static ref PROPAGATION_TYPES: MsFlags = MsFlags::MS_SHARED
        .bitor(MsFlags::MS_PRIVATE)
        .bitor(MsFlags::MS_SLAVE)
        .bitor(MsFlags::MS_UNBINDABLE);
    static ref MS_PROPAGATION: MsFlags = PROPAGATION_TYPES
        .bitor(MsFlags::MS_REC)
        .bitor(MsFlags::MS_SILENT);
    static ref MS_BIND_RO: MsFlags = MsFlags::MS_BIND.bitor(MsFlags::MS_RDONLY);
}

pub fn mount(
    fs_type: Option<&str>,
    source: Option<&str>,
    options: &[String],
    target: &str,
) -> Result<()> {
    let (flags, data) = parse_options(options);

    let opt = data.join(",");
    if opt.len() > MNT_OPTION_MAX_LEN {
        return Err(anyhow!("mount option is too long"));
    }

    let data = if !data.is_empty() {
        Some(opt.as_str())
    } else {
        None
    };

    // mount with non-propagation first, or remount with changed data
    let oflags = flags.bitand(PROPAGATION_TYPES.not());
    let zero: MsFlags = MsFlags::from_bits(0).unwrap();
    if flags.bitand(MsFlags::MS_REMOUNT).eq(&zero) || data.is_some() {
        nix::mount::mount(source, target, fs_type, oflags, data)
            .map_err(|e| anyhow!("failed to mount {:?} to {}, err: {}", source, target, e))?
    }

    // change the propagation type
    if flags.bitand(*PROPAGATION_TYPES).ne(&zero) {
        nix::mount::mount::<str, str, str, str>(
            None,
            target,
            None,
            flags.bitand(*MS_PROPAGATION),
            None,
        )
        .map_err(|e| anyhow!("failed change mount propagation of {}, err: {}", target, e))?
    }

    if oflags.bitand(*MS_BIND_RO).eq(&MS_BIND_RO) {
        nix::mount::mount::<str, str, str, str>(
            None,
            target,
            None,
            oflags.bitor(MsFlags::MS_REMOUNT),
            None,
        )
        .map_err(|e| anyhow!("failed change read only of {}, err: {}", target, e))?;
    }
    Ok(())
}

pub fn bind_mount<T: AsRef<Path>>(source: T, target: &str, options: &[String]) -> Result<()> {
    let (flags, data) = parse_options(options);
    let opts = if data.is_empty() {
        None
    } else {
        Some(data.join(","))
    };
    nix::mount::mount(
        Some(source.as_ref()),
        target,
        Some("bind"),
        flags.bitor(MsFlags::MS_BIND),
        opts.as_deref(),
    )
    .map_err(|e| anyhow!("failed to mount {}, {}", target, e))?;
    // For readonly bind mounts, we need to remount with the readonly flag.
    // This is needed as only very recent versions of libmount/util-linux support "bind,ro"
    if flags.contains(MsFlags::MS_RDONLY) {
        nix::mount::mount(
            Some(source.as_ref()),
            target,
            Some("bind"),
            flags.bitor(MsFlags::MS_BIND).bitor(MsFlags::MS_REMOUNT),
            opts.as_deref(),
        )
        .map_err(|e| anyhow!("failed to mount {} as read only, {}", target, e))?;
    }
    Ok(())
}

pub fn unmount(target: &str, flags: i32) -> Result<()> {
    let res = target
        .with_nix_path(|cstr| unsafe { libc::umount2(cstr.as_ptr(), flags) })
        .map_err(|e| anyhow!("failed to umount {}, {}", target, e))?;
    let err = Errno::result(res).map(drop);
    match err {
        Ok(_) => Ok(()),
        Err(e) => {
            if e == Errno::ENOENT {
                debug!("the umount path {} not exist", target);
                return Ok(());
            }
            Err(anyhow!("failed to umount {}, {}", target, e))
        }
    }
}

fn parse_options(options: &[String]) -> (MsFlags, Vec<String>) {
    let mut flags: MsFlags = MsFlags::from_bits(0).unwrap();
    let mut data: Vec<String> = Vec::new();
    let mut handle = |x: String| {
        if let Some(f) = MOUNT_FLAGS.get(x.as_str()) {
            if f.clear {
                flags.bitand_assign(f.flags.not());
            } else {
                flags.bitor_assign(f.flags)
            }
        } else {
            data.push(x)
        }
    };
    options.iter().for_each(|x| {
        handle(x.to_string());
    });
    (flags, data)
}

pub fn get_mount_type(mount_point: &str) -> Result<String> {
    let mount_stats = read_to_string("/proc/self/mountstats")
        .map_err(|e| anyhow!("failed to open /proc/self/mountstats, err:{}", e))?;

    for line in mount_stats.lines() {
        let mount_type = match Regex::new(
            format!("device .+ mounted on {} with fstype (.+)", mount_point).as_str(),
        )?
        .captures(line)
        {
            Some(c) => c,
            None => continue,
        };

        if mount_type.len() > 1 {
            return Ok(mount_type[1].to_string());
        }
    }

    Err(anyhow!("get type for mount point {} failed", mount_point))
}
