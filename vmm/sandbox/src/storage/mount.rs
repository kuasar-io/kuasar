/*
Copyright 2022 The Kuasar Authors.

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

use anyhow::anyhow;
use containerd_sandbox::{error::Result, spec::Mount};
use vmm_common::DEV_SHM;

use crate::{storage::MountInfo, utils::read_file};

pub fn is_bind_shm(m: &Mount) -> bool {
    is_bind(m) && m.destination == DEV_SHM
}

pub fn is_bind(m: &Mount) -> bool {
    m.r#type == "bind" && !m.source.is_empty()
}

pub fn is_overlay(m: &Mount) -> bool {
    m.r#type == "overlay"
}

pub async fn get_mount_info(mount_point: &str) -> Result<Option<MountInfo>> {
    if mount_point.is_empty() {
        return Ok(None);
    }
    let mounts = read_file("/proc/self/mountinfo").await?;
    parse_mount_info_content(&mounts, mount_point)
}

fn is_parent_or_eq(parent: &str, child: &str) -> bool {
    if parent == child {
        return true;
    }
    if parent == "/" {
        return true;
    }
    // Check that child starts with parent and the next character is '/',
    // preventing false matches like "/mnt/shared" matching "/mnt/shared_data".
    child.starts_with(parent)
        && child
            .as_bytes()
            .get(parent.len())
            .is_some_and(|&b| b == b'/')
}

fn parse_mount_info_content(content: &str, mount_point: &str) -> Result<Option<MountInfo>> {
    let mut best_match: Option<MountInfo> = None;
    for line in content.lines() {
        let fields = line.split_whitespace().collect::<Vec<&str>>();
        if fields.len() < 7 {
            continue;
        }
        let mp = fields[4].to_string();
        if is_parent_or_eq(&mp, mount_point) {
            if let Some(ref prev) = best_match {
                if mp.len() <= prev.mount_point.len() {
                    continue;
                }
            }
            let mut sep_idx = None;
            for (i, &f) in fields.iter().enumerate().skip(6) {
                if f == "-" {
                    sep_idx = Some(i);
                    break;
                }
            }
            let sep_idx = match sep_idx {
                Some(idx) => idx,
                None => {
                    return Err(
                        anyhow!("the line '{}' in mountinfo has no separator '-'", line).into(),
                    )
                }
            };
            if fields.len() < sep_idx + 4 {
                return Err(anyhow!("the line '{}' in mountinfo has format error", line).into());
            }

            let fs_type = fields[sep_idx + 1].to_string();
            let mut options: Vec<String> = Vec::new();
            for option in fields[5].split(',').chain(fields[sep_idx + 3].split(',')) {
                if !option.is_empty() && !options.iter().any(|o| o == option) {
                    options.push(option.to_string());
                }
            }

            // Extract optional fields (propagation settings)
            let mut shared = false;
            let mut slave = false;
            let mut unbindable = false;
            for opt_field in fields.iter().take(sep_idx).skip(6) {
                if opt_field.starts_with("shared:") || *opt_field == "shared" {
                    shared = true;
                } else if opt_field.starts_with("master:")
                    || opt_field.starts_with("propagate_from:")
                    || *opt_field == "slave"
                {
                    slave = true;
                } else if *opt_field == "unbindable" {
                    unbindable = true;
                }
            }

            if shared {
                options.push("shared".to_string());
            } else if slave {
                options.push("slave".to_string());
            } else if unbindable {
                options.push("unbindable".to_string());
            } else {
                options.push("private".to_string());
            }

            best_match = Some(MountInfo {
                mount_point: mp,
                fs_type,
                options,
            });
        }
    }
    Ok(best_match)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mount_info_content() {
        struct TestCase {
            name: &'static str,
            content: &'static str,
            mount_point: &'static str,
            expected: Option<(&'static str, &'static str, Vec<&'static str>)>,
        }

        let cases = vec![
            TestCase {
                name: "shared mount",
                content: "36 35 98:0 / /mnt/shared rw,relatime shared:1 - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/shared",
                expected: Some(("/mnt/shared", "ext4", vec!["rw", "relatime", "shared"])),
            },
            TestCase {
                name: "subpath under shared mount",
                content: "36 35 98:0 / /mnt/shared rw,relatime shared:1 - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/shared/sub/dir",
                expected: Some(("/mnt/shared", "ext4", vec!["rw", "relatime", "shared"])),
            },
            TestCase {
                name: "longest prefix match",
                content: "21 20 98:0 / / rw,relatime shared:2 - ext4 /dev/vda2 rw\n36 35 98:0 / /mnt/shared rw,relatime shared:1 - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/shared/sub/dir",
                expected: Some(("/mnt/shared", "ext4", vec!["rw", "relatime", "shared"])),
            },
            TestCase {
                name: "slave mount",
                content: "36 35 98:0 / /mnt/slave rw,relatime master:7 - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/slave",
                expected: Some(("/mnt/slave", "ext4", vec!["rw", "relatime", "slave"])),
            },
            TestCase {
                name: "unbindable mount",
                content: "36 35 98:0 / /mnt/unbind rw,relatime unbindable - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/unbind",
                expected: Some(("/mnt/unbind", "ext4", vec!["rw", "relatime", "unbindable"])),
            },
            TestCase {
                name: "private mount (no optional fields)",
                content: "36 35 98:0 / /mnt/private rw,relatime - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/private",
                expected: Some(("/mnt/private", "ext4", vec!["rw", "relatime", "private"])),
            },
            TestCase {
                name: "tmpfs size from super options",
                content: "36 35 0:77 / /tmp/kubernetes.io~empty-dir-test rw,nosuid,nodev shared:1 - tmpfs tmpfs rw,size=65536k,mode=755,inode64\n",
                mount_point: "/tmp/kubernetes.io~empty-dir-test",
                expected: Some((
                    "/tmp/kubernetes.io~empty-dir-test",
                    "tmpfs",
                    vec![
                        "rw",
                        "nosuid",
                        "nodev",
                        "size=65536k",
                        "mode=755",
                        "inode64",
                        "shared",
                    ],
                )),
            },
            TestCase {
                name: "non-existent mount point (no root)",
                content: "36 35 98:0 / /mnt/other rw,relatime - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/private",
                expected: None,
            },
            TestCase {
                name: "fallback to root mount",
                content: "21 1 98:0 / / rw,relatime shared:1 - ext4 /dev/vda2 rw\n36 35 98:0 / /mnt/other rw,relatime - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/private",
                expected: Some(("/", "ext4", vec!["rw", "relatime", "shared"])),
            },
            TestCase {
                name: "similar prefix should not match",
                content: "36 35 98:0 / /mnt/shared rw,relatime shared:1 - ext4 /dev/vda1 rw\n",
                mount_point: "/mnt/shared_data",
                expected: None,
            },
        ];

        for case in cases {
            let res = parse_mount_info_content(case.content, case.mount_point);
            assert!(res.is_ok(), "case '{}' failed: {:?}", case.name, res.err());
            let opt = res.unwrap();
            match (opt, case.expected) {
                (None, None) => {}
                (Some(mi), Some((mp, fs, opts))) => {
                    assert_eq!(mi.mount_point, mp, "case '{}'", case.name);
                    assert_eq!(mi.fs_type, fs, "case '{}'", case.name);
                    let expected_opts: Vec<String> = opts.iter().map(|s| s.to_string()).collect();
                    assert_eq!(mi.options, expected_opts, "case '{}'", case.name);
                }
                (res, exp) => {
                    panic!(
                        "case '{}' mismatch: got {:?}, expected {:?}",
                        case.name, res, exp
                    );
                }
            }
        }
    }
}
