/*
Copyright 2026 The Kuasar Authors.

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

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum SandboxTarget {
    CloudHypervisor { dir: PathBuf },
}

pub fn resolve_sandbox(id_prefix: &str) -> Result<SandboxTarget> {
    let bases = [Path::new("/run/kuasar-vmm")];
    resolve_sandbox_with_bases(id_prefix, &bases)
}

pub fn resolve_sandbox_with_bases(id_prefix: &str, bases: &[&Path]) -> Result<SandboxTarget> {
    let p = Path::new(id_prefix);
    if p.is_absolute() && p.exists() {
        let canon_p = std::fs::canonicalize(p)
            .with_context(|| format!("failed to canonicalize absolute sandbox path {:?}", p))?;
        // Only accept absolute paths under Cloud Hypervisor sandbox roots.
        for base in bases {
            if !base.exists() {
                continue;
            }
            let canon_base = std::fs::canonicalize(base).with_context(|| {
                format!("failed to canonicalize sandbox base directory {:?}", base)
            })?;
            if canon_p.starts_with(&canon_base) {
                return Ok(SandboxTarget::CloudHypervisor {
                    dir: canon_p.clone(),
                });
            }
        }
        return Err(anyhow!(
            "absolute path {:?} is not under Cloud Hypervisor sandbox roots {:?}",
            canon_p,
            bases
        ));
    }

    let mut matches = vec![];

    for base in bases {
        if !base.exists() {
            continue;
        }

        let entries = std::fs::read_dir(base)
            .with_context(|| format!("failed to read sandbox base directory {:?}", base))?;
        for entry in entries {
            let entry = entry
                .with_context(|| format!("failed to read entry under sandbox base {:?}", base))?;
            let ft = entry.file_type().with_context(|| {
                format!(
                    "failed to read file type for sandbox entry {:?}",
                    entry.path()
                )
            })?;
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(id_prefix) {
                        matches.push(entry.path());
                    }
                }
            }
        }
    }
    match matches.len() {
        0 => Err(anyhow!(
            "no Cloud Hypervisor sandbox found with prefix {:?}",
            id_prefix
        )),
        1 => Ok(SandboxTarget::CloudHypervisor {
            dir: matches.pop().unwrap(),
        }),
        _ => Err(anyhow!(
            "ambiguous Cloud Hypervisor sandbox prefix {:?}, found: {:?}",
            id_prefix,
            matches
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>()
        )),
    }
}

pub fn get_hvsock_path(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join("task.vsock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_resolve_sandbox() {
        let temp_dir = tempdir().unwrap();
        let vmm_base = temp_dir.path().join("kuasar-vmm");

        fs::create_dir_all(&vmm_base).unwrap();

        let id1 = "a1b2c3d4e5f6";
        let id2 = "a1b2c3d4e599";
        let id3 = "f0e1d2c3b4a5";

        fs::create_dir(vmm_base.join(id1)).unwrap();
        fs::create_dir(vmm_base.join(id2)).unwrap();
        fs::create_dir(vmm_base.join(id3)).unwrap();

        let bases = vec![vmm_base.as_path()];

        struct TestCase {
            name: &'static str,
            input: String,
            expected_dir: Option<PathBuf>,
            expected_err: Option<&'static str>,
        }

        let cases = vec![
            TestCase {
                name: "exact_match",
                input: id1.to_string(),
                expected_dir: Some(vmm_base.join(id1)),
                expected_err: None,
            },
            TestCase {
                name: "prefix_match_unique",
                input: "f0e1".to_string(),
                expected_dir: Some(vmm_base.join(id3)),
                expected_err: None,
            },
            TestCase {
                name: "ambiguous_prefix",
                input: "a1b2c3d4e5".to_string(),
                expected_dir: None,
                expected_err: Some("ambiguous Cloud Hypervisor sandbox prefix"),
            },
            TestCase {
                name: "not_found",
                input: "deadbeef".to_string(),
                expected_dir: None,
                expected_err: Some("no Cloud Hypervisor sandbox found"),
            },
            TestCase {
                name: "absolute_path",
                input: vmm_base.join(id1).to_str().unwrap().to_string(),
                expected_dir: Some(vmm_base.join(id1)),
                expected_err: None,
            },
        ];

        for case in cases {
            let res = resolve_sandbox_with_bases(&case.input, &bases);
            match (res, case.expected_dir, case.expected_err) {
                (Ok(SandboxTarget::CloudHypervisor { dir }), Some(expected), None) => {
                    assert_eq!(dir, expected, "Test Case '{}' failed", case.name);
                }
                (Err(e), None, Some(err_msg)) => {
                    assert!(
                        e.to_string().contains(err_msg),
                        "Test Case '{}' error message MISMATCH. Got: '{}', Expected to contain: '{}'",
                        case.name,
                        e,
                        err_msg
                    );
                }
                (res, _, _) => panic!(
                    "Test Case '{}' failed: unexpected result {:?}",
                    case.name, res
                ),
            }
        }
    }

    #[test]
    fn test_rejects_absolute_path_outside_cloud_hypervisor_roots() {
        let temp_dir = tempdir().unwrap();
        let vmm_base = temp_dir.path().join("kuasar-vmm");
        let other_base = temp_dir.path().join("other");
        let sandbox_dir = other_base.join("sandbox-1");

        fs::create_dir_all(&vmm_base).unwrap();
        fs::create_dir_all(&sandbox_dir).unwrap();

        let bases = vec![vmm_base.as_path()];
        let err = resolve_sandbox_with_bases(sandbox_dir.to_str().unwrap(), &bases).unwrap_err();
        assert!(err
            .to_string()
            .contains("is not under Cloud Hypervisor sandbox roots"));
    }

    #[test]
    fn test_rejects_lexically_nested_absolute_path_outside_cloud_hypervisor_roots() {
        let temp_dir = tempdir().unwrap();
        let vmm_base = temp_dir.path().join("kuasar-vmm");
        let other_base = temp_dir.path().join("other");
        let sandbox_dir = other_base.join("sandbox-1");
        let tricky_path = vmm_base.join("..").join("other").join("sandbox-1");

        fs::create_dir_all(&vmm_base).unwrap();
        fs::create_dir_all(&sandbox_dir).unwrap();

        let bases = vec![vmm_base.as_path()];
        let err = resolve_sandbox_with_bases(tricky_path.to_str().unwrap(), &bases).unwrap_err();
        assert!(err
            .to_string()
            .contains("is not under Cloud Hypervisor sandbox roots"));
    }
}
