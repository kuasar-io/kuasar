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

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::{template::SnapshotType, utils::write_file_atomic, vm::SnapshotPathOverrides};

/// Minimal metadata written alongside a template snapshot for pool restore.
#[derive(Debug, Serialize, Deserialize)]
pub struct TemplateMeta {
    pub id: String,
    pub snapshot_dir: PathBuf,
    pub original_task_vsock: String,
    pub original_console_path: String,
}

impl TemplateMeta {
    /// Serialize to `{dir}/metadata.json` atomically.
    pub async fn save(&self, dir: &Path) -> Result<()> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| anyhow!("serialize TemplateMeta: {}", e))?;
        write_file_atomic(&dir.join("metadata.json"), &content).await
    }

    /// Deserialize from `{dir}/metadata.json`.
    pub async fn load(dir: &Path) -> Result<Self> {
        let content = tokio::fs::read_to_string(dir.join("metadata.json"))
            .await
            .map_err(|e| anyhow!("read metadata.json from {}: {}", dir.display(), e))?;
        serde_json::from_str(&content).map_err(|e| anyhow!("parse metadata.json: {}", e).into())
    }
}

/// Patch the CH `state.json` in `work_dir` for snapshot types that hotplug fresh network
/// devices after restore.
///
/// Returns true for keys that belong to a virtio-net device or its PCI transport wrapper.
///
/// Kuasar assigns `intf-{link_index}` to every tap-backed virtio-net device
/// (see `network/link.rs`).  Cloud-hypervisor wraps each virtio device in a PCI
/// transport named `_virtio-pci-{id}`.  Both patterns are exclusive to network
/// devices, so matching by prefix is safe.
fn is_net_device_key(key: &str) -> bool {
    key.starts_with("intf-") || key.starts_with("_virtio-pci-intf-")
}

/// Strips all net device entries from a copy of the template's `state.json`.
///
/// `prepare_memory_backend` places a symlink at `work_dir/state.json` pointing to the
/// template's state file.  CH's restore process loads this file into its device registry,
/// which means any network device saved in the snapshot ends up registered even though we
/// stripped it from `config.json`.  The result is a zombie entry: present in CH's device ID
/// registry but missing from the PCI bus, which makes `vm.add-net` fail with
/// `IdentifierNotUnique`.
///
/// We detect net devices by key prefix (`intf-*` / `_virtio-pci-intf-*`) rather than by the
/// caller-supplied hotplug IDs, because those IDs reflect the *new* sandbox's host link index
/// and may differ from the index recorded in the template snapshot.
///
/// CH's state.json stores net device state in two locations, both of which are patched:
///
/// 1. `snapshots["device-manager"]["snapshots"]["intf-N"]` and the companion PCI wrapper
///    `"_virtio-pci-intf-N"` — object keys removed directly.
///
/// 2. `snapshots["device-manager"]["snapshot_data"]["state"]` — a JSON-encoded *string*
///    containing the `device_tree` object, where the same keys also appear.
///    This string is parsed, patched, and re-serialized in place.
///
/// The symlink is replaced with a regular file so the template's original state.json is
/// never modified.
pub async fn patch_snapshot_state(work_dir: &Path) -> Result<()> {
    let state_path = work_dir.join("state.json");
    let content = tokio::fs::read_to_string(&state_path)
        .await
        .map_err(|e| anyhow!("read state.json: {}", e))?;
    let mut state: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse state.json: {}", e))?;

    let removed = remove_net_device_state(&mut state);
    if removed == 0 {
        warn!(
            "patch_snapshot_state: no net device entries found in {}",
            state_path.display()
        );
        return Ok(());
    }
    info!(
        "patch_snapshot_state: removed {} net device entries from {}",
        removed,
        state_path.display()
    );

    let patched = serde_json::to_string_pretty(&state)
        .map_err(|e| anyhow!("serialize patched state.json: {}", e))?;

    // The symlink must be removed before writing the patched content; writing through a
    // symlink would modify the template's original state.json.
    tokio::fs::remove_file(&state_path)
        .await
        .map_err(|e| anyhow!("remove state.json symlink: {}", e))?;
    write_file_atomic(&state_path, &patched).await
}

/// Recursively removes net device entries from a CH state.json value tree.
///
/// Handles three cases:
/// - Object:  removes keys matched by `is_net_device_key` (covers `device_tree` and
///            `snapshots["device-manager"]["snapshots"]`).
/// - Array:   removes elements whose `"id"` field matches (legacy format).
/// - String:  if the string is valid JSON, parses it, patches it, and re-serializes in place
///            (covers `snapshot_data.state` which embeds `device_tree` as a JSON string).
///
/// Returns the total count of entries removed across all locations.
fn remove_net_device_state(value: &mut serde_json::Value) -> usize {
    let mut count = 0;
    match value {
        serde_json::Value::Array(arr) => {
            let before = arr.len();
            arr.retain(|item| match item.get("id").and_then(|v| v.as_str()) {
                Some(id) => !is_net_device_key(id),
                None => true,
            });
            count += before - arr.len();
            for item in arr.iter_mut() {
                count += remove_net_device_state(item);
            }
        }
        serde_json::Value::Object(map) => {
            let before = map.len();
            map.retain(|k, _| !is_net_device_key(k));
            count += before - map.len();
            for v in map.values_mut() {
                count += remove_net_device_state(v);
            }
        }
        serde_json::Value::String(s) => {
            // CH stores device-manager sub-state (including device_tree) as a JSON string
            // inside snapshot_data.state.  Parse it, patch it, and write it back.
            if let Ok(mut inner) = serde_json::from_str::<serde_json::Value>(s) {
                let inner_removed = remove_net_device_state(&mut inner);
                if inner_removed > 0 {
                    if let Ok(reserialised) = serde_json::to_string(&inner) {
                        *s = reserialised;
                    }
                    count += inner_removed;
                }
            }
        }
        _ => {}
    }
    count
}

pub async fn validate_snapshot_config(src: &Path, snapshot_type: &SnapshotType) -> Result<()> {
    let content = tokio::fs::read_to_string(src)
        .await
        .map_err(|e| anyhow!("read {}: {}", src.display(), e))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse config.json: {}", e))?;

    if cfg
        .pointer("/vsock/socket")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return Err(
            anyhow!("config.json missing /vsock/socket — unexpected CH config format").into(),
        );
    }

    if matches!(snapshot_type, SnapshotType::Environment) && has_network_devices(&cfg) {
        return Err(anyhow!(
            "Environment snapshot config must not contain network devices; restore requires network hotplug"
        )
        .into());
    }

    Ok(())
}

/// Read the `id` field of each net device in a CH `config.json` file.
///
/// Returns an empty vec if there are no net devices or the file can't be parsed.
/// Used by WarmFork/Continuation restore to obtain the device IDs from the snapshot so
/// they can be passed back to CH's `vm.restore` `net_fds` parameter (which matches by ID).
pub async fn read_net_device_ids(config_json: &Path) -> Result<Vec<String>> {
    let content = tokio::fs::read_to_string(config_json)
        .await
        .map_err(|e| anyhow!("read {}: {}", config_json.display(), e))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse config.json: {}", e))?;
    let ids = match cfg.get("net") {
        Some(serde_json::Value::Array(devices)) => devices
            .iter()
            .filter_map(|d| d.get("id")?.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    };
    Ok(ids)
}

/// Per-device metadata read from a CH `config.json` net array entry.
/// Used by Continuation restore to re-open existing tap devices.
pub struct NetDeviceConfig {
    pub id: String,
    pub mac: String,
    /// Total virtio queue count (rx + tx); number of tap FDs = num_queues / 2.
    pub num_queues: u32,
}

/// Read id, mac, and num_queues for each net device in a CH `config.json` file.
pub async fn read_net_device_configs(config_json: &Path) -> Result<Vec<NetDeviceConfig>> {
    let content = tokio::fs::read_to_string(config_json)
        .await
        .map_err(|e| anyhow!("read {}: {}", config_json.display(), e))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse config.json: {}", e))?;
    let configs = match cfg.get("net") {
        Some(serde_json::Value::Array(devices)) => devices
            .iter()
            .filter_map(|d| {
                let id = d.get("id")?.as_str()?.to_owned();
                let mac = d.get("mac")?.as_str()?.to_owned();
                let num_queues = d.get("num_queues").and_then(|v| v.as_u64()).unwrap_or(2) as u32;
                Some(NetDeviceConfig {
                    id,
                    mac,
                    num_queues,
                })
            })
            .collect(),
        _ => vec![],
    };
    Ok(configs)
}

fn has_network_devices(cfg: &serde_json::Value) -> bool {
    match cfg.get("net") {
        Some(serde_json::Value::Array(devices)) => !devices.is_empty(),
        Some(serde_json::Value::Object(device)) => !device.is_empty(),
        Some(serde_json::Value::Null) | None => false,
        Some(other) => {
            warn!(
                "has_network_devices: unexpected 'net' value shape: {:?}, treating as no devices",
                other
            );
            false
        }
    }
}

/// Patch a Cloud Hypervisor `config.json` by updating sandbox-specific socket and log paths.
///
/// CH's config.json records absolute paths for the vsock and console devices, which are unique
/// per-sandbox.  During restore these must point to the *new* sandbox's paths, not the template's.
/// pmem (rootfs) paths are deliberately left unchanged — they are shared read-only.
///
/// `disk_remaps` controls how hot-plugged disk entries are handled:
/// - Empty (template mode): all disk entries are stripped.  Containers will re-hot-plug their
///   own `.img` files after the VM starts.
/// - Non-empty (full-checkpoint mode): each `(device_id, new_path)` pair remaps the `path`
///   field of the matching disk entry to point to the restored copy in the new sandbox dir.
///   Disk entries whose `id` is not in the remap list are stripped (conservative).
pub async fn patch_snapshot_config(
    src: &Path,
    dst: &Path,
    overrides: &SnapshotPathOverrides,
    disk_remaps: &[(String, String)],
    snapshot_type: &SnapshotType,
) -> Result<()> {
    let content = tokio::fs::read_to_string(src)
        .await
        .map_err(|e| anyhow!("read {}: {}", src.display(), e))?;
    let mut cfg: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| anyhow!("parse config.json: {}", e))?;

    // vsock socket path — required; fail early if structure is unexpected.
    // CH config.json: vsock is a top-level field with a "socket" key (not under "payload").
    match cfg.pointer_mut("/vsock/socket") {
        Some(v) => *v = serde_json::Value::String(overrides.task_vsock.clone()),
        None => {
            return Err(
                anyhow!("config.json missing /vsock/socket — unexpected CH config format").into(),
            )
        }
    }

    // console log file — optional (may not be present in all CH configs)
    if let Some(v) = cfg.pointer_mut("/console/file") {
        *v = serde_json::Value::String(overrides.console_path.clone());
    }

    if disk_remaps.is_empty() {
        // Template mode: strip all hot-plugged container blk devices.
        // Containers re-attach their own `.img` files via hot-plug after the VM starts.
        if let Some(disks) = cfg.get_mut("disks") {
            *disks = serde_json::Value::Array(vec![]);
        }
    } else {
        // Full-checkpoint mode: remap each disk's path to the restored copy in the new sandbox
        // dir.  Disk entries whose id is absent from disk_remaps are stripped (conservative).
        if let Some(arr) = cfg.get_mut("disks").and_then(|d| d.as_array_mut()) {
            arr.retain_mut(|disk| {
                let id = match disk.get("id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => return false,
                };
                match disk_remaps.iter().find(|(did, _)| *did == id) {
                    Some((_, new_path)) => {
                        disk["path"] = serde_json::Value::String(new_path.clone());
                        true
                    }
                    None => false,
                }
            });
        }
    }

    // Strip network devices for Environment (hotplug path): CH would require re-providing the
    // original tap FDs, which we don't do for Environment; removing them avoids that requirement.
    if snapshot_type.requires_network_hotplug() {
        if let Some(net) = cfg.get_mut("net") {
            *net = serde_json::Value::Array(vec![]);
        }
    }

    // WarmFork: keep net devices intact (state.json preserved → device restores to DRIVER_OK)
    // but strip the "tap" name field from each net device.
    //
    // CH's device_manager checks net_cfg.tap BEFORE net_cfg.fds. If "tap" is present it calls
    // Tap::open_named(old_tap_name) — opening or creating a NEW tap by the snapshot's name,
    // which has no TC redirect rules and is disconnected from the new pod's veth.  Removing "tap"
    // forces CH down the else-if-fds branch (Net::from_tap_fds), which uses the new FDs we
    // supply via vm.restore net_fds (SCM_RIGHTS), pointing to the correctly wired tap.
    if matches!(snapshot_type, SnapshotType::WarmFork) {
        if let Some(net_arr) = cfg.get_mut("net").and_then(|n| n.as_array_mut()) {
            for dev in net_arr.iter_mut() {
                if let Some(obj) = dev.as_object_mut() {
                    obj.remove("tap");
                }
            }
        }
    }

    // Continuation: the sandbox netns and tap device (IFF_PERSIST) survive CH death.
    // Strip "tap" so CH uses from_tap_fds (same as WarmFork) rather than Tap::open_named.
    // Tap::open_named may omit IFF_VNET_HDR, causing frame-format mismatch that breaks ARP.
    // Kuasar re-opens the existing tap with correct flags and passes the FDs via net_fds.
    // The "fds" field is kept so CH's RestoreConfig::validate() requires a net_fds entry.
    if matches!(snapshot_type, SnapshotType::Continuation) {
        if let Some(net_arr) = cfg.get_mut("net").and_then(|n| n.as_array_mut()) {
            for dev in net_arr.iter_mut() {
                if let Some(obj) = dev.as_object_mut() {
                    obj.remove("tap");
                }
            }
        }
    }

    let serialized = serde_json::to_string_pretty(&cfg)
        .map_err(|e| anyhow!("serialize patched config.json: {}", e))?;
    write_file_atomic(dst, &serialized).await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use temp_dir::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_patch_snapshot_config() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        // Mirrors actual CH config.json: vsock and console are top-level fields.
        // "payload" in CH config is only for kernel/cmdline/initramfs.
        // "disks" contains hot-plugged container blk images (per-sandbox, must be stripped).
        let original = serde_json::json!({
            "payload": {
                "kernel": "/var/lib/kuasar/vmlinux.bin",
                "cmdline": "console=hvc0 root=/dev/pmem0p1 ro"
            },
            "vsock": {
                "socket": "/old/sandbox-abc/task.vsock",
                "cid": 3,
                "iommu": false
            },
            "console": {
                "file": "/tmp/sandbox-abc-task.log",
                "mode": "File"
            },
            "pmem": [
                {"file": "/var/lib/kuasar/rootfs.img", "discard_writes": true}
            ],
            "disks": [
                {"path": "/old/sandbox-abc/container-1.img", "readonly": false}
            ]
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&original).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/sandbox-xyz/task.vsock".to_string(),
            console_path: "/tmp/sandbox-xyz-task.log".to_string(),
        };
        // Template mode: empty disk_remaps → disks stripped
        patch_snapshot_config(&src, &dst, &overrides, &[], &SnapshotType::WarmFork)
            .await
            .unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&dst).await.unwrap()).unwrap();

        assert_eq!(patched["vsock"]["socket"], "/new/sandbox-xyz/task.vsock");
        assert_eq!(patched["console"]["file"], "/tmp/sandbox-xyz-task.log");
        // pmem path must remain unchanged
        assert_eq!(patched["pmem"][0]["file"], "/var/lib/kuasar/rootfs.img");
        // template mode: container blk devices stripped (re-hot-plugged after restore)
        assert_eq!(patched["disks"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_disk_remap() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        let original = serde_json::json!({
            "vsock": {"socket": "/old/task.vsock", "cid": 3},
            "console": {"file": "/tmp/old.log", "mode": "File"},
            "pmem": [{"file": "/var/lib/kuasar/rootfs.img", "discard_writes": true}],
            "disks": [
                {"path": "/old/storage3.img", "readonly": false, "id": "blk3"},
                {"path": "/old/storage4.img", "readonly": false, "id": "blk4"}
            ]
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&original).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/task.vsock".to_string(),
            console_path: "/tmp/new.log".to_string(),
        };
        // Full-checkpoint mode: remap blk3, strip blk4 (not in remaps)
        let remaps = vec![("blk3".to_string(), "/new/sandbox/storage3.img".to_string())];
        patch_snapshot_config(&src, &dst, &overrides, &remaps, &SnapshotType::Continuation)
            .await
            .unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&dst).await.unwrap()).unwrap();

        assert_eq!(patched["vsock"]["socket"], "/new/task.vsock");
        // blk3 remapped to new path
        let disks = patched["disks"].as_array().unwrap();
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0]["id"], "blk3");
        assert_eq!(disks[0]["path"], "/new/sandbox/storage3.img");
        // pmem unchanged
        assert_eq!(patched["pmem"][0]["file"], "/var/lib/kuasar/rootfs.img");
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_continuation_strips_tap_keeps_fds() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        let original = serde_json::json!({
            "vsock": {"socket": "/old/task.vsock", "cid": 3},
            "net": [{"id": "intf-2", "tap": "tap_kua_1", "mac": "aa:bb:cc:dd:ee:ff", "fds": [5]}],
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&original).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/task.vsock".to_string(),
            console_path: "/tmp/new.log".to_string(),
        };
        patch_snapshot_config(&src, &dst, &overrides, &[], &SnapshotType::Continuation)
            .await
            .unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&dst).await.unwrap()).unwrap();

        let net = patched["net"].as_array().expect("net must be an array");
        assert_eq!(
            net.len(),
            1,
            "net device must be preserved for Continuation"
        );
        assert!(
            net[0].get("tap").is_none() || net[0]["tap"].is_null(),
            "tap field must be stripped so CH uses from_tap_fds (avoids IFF_VNET_HDR mismatch)"
        );
        assert!(
            net[0].get("fds").is_some() && !net[0]["fds"].is_null(),
            "fds field must be kept so CH validation requires a net_fds entry"
        );
        assert_eq!(net[0]["id"], "intf-2", "device id must be preserved");
        assert_eq!(net[0]["mac"], "aa:bb:cc:dd:ee:ff", "mac must be preserved");
    }

    #[tokio::test]
    async fn test_template_meta_save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let meta = TemplateMeta {
            id: "tmpl-001".to_string(),
            snapshot_dir: PathBuf::from("/var/lib/kuasar/templates/tmpl-001/snapshot"),
            original_task_vsock: "/var/lib/kuasar/templates/tmpl-001/task.vsock".to_string(),
            original_console_path: "/tmp/tmpl-001-task.log".to_string(),
        };

        meta.save(dir.path()).await.unwrap();

        let loaded = TemplateMeta::load(dir.path()).await.unwrap();
        assert_eq!(loaded.id, meta.id);
        assert_eq!(loaded.snapshot_dir, meta.snapshot_dir);
        assert_eq!(loaded.original_task_vsock, meta.original_task_vsock);
        assert_eq!(loaded.original_console_path, meta.original_console_path);
    }

    #[tokio::test]
    async fn test_template_meta_load_missing_returns_error() {
        let dir = TempDir::new().unwrap();
        let result = TemplateMeta::load(dir.path()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("metadata.json"));
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_missing_vsock_returns_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        let cfg = serde_json::json!({ "pmem": [], "console": { "mode": "Null" } });
        tokio::fs::write(&src, serde_json::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/task.vsock".to_string(),
            console_path: "/tmp/console.log".to_string(),
        };
        let result =
            patch_snapshot_config(&src, &dst, &overrides, &[], &SnapshotType::WarmFork).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("missing /vsock/socket"), "got: {}", msg);
    }

    #[tokio::test]
    async fn test_validate_environment_snapshot_rejects_network_devices() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let cfg = serde_json::json!({
            "vsock": {"socket": "/old/task.vsock", "cid": 3},
            "net": [{"id": "tap0", "tap": "tap0"}]
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        let result = validate_snapshot_config(&src, &SnapshotType::Environment).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("network devices"));
    }

    #[tokio::test]
    async fn test_patch_snapshot_config_warmfork_strips_tap_field() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let dst = dir.path().join("config_patched.json");

        let original = serde_json::json!({
            "vsock": {"socket": "/old/task.vsock", "cid": 3},
            "console": {"file": "/tmp/old.log", "mode": "File"},
            "net": [{"id": "intf-2", "tap": "tap_kua_1", "mac": "aa:bb:cc:dd:ee:ff", "fds": [5]}],
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&original).unwrap())
            .await
            .unwrap();

        let overrides = SnapshotPathOverrides {
            task_vsock: "/new/task.vsock".to_string(),
            console_path: "/tmp/new.log".to_string(),
        };
        patch_snapshot_config(&src, &dst, &overrides, &[], &SnapshotType::WarmFork)
            .await
            .unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&dst).await.unwrap()).unwrap();

        let net = patched["net"].as_array().expect("net must be an array");
        assert_eq!(net.len(), 1, "net device must be preserved for WarmFork");
        assert!(
            net[0].get("tap").is_none() || net[0]["tap"].is_null(),
            "tap field must be stripped so CH uses the fds path"
        );
        assert_eq!(net[0]["id"], "intf-2", "device id must be preserved");
        assert_eq!(net[0]["mac"], "aa:bb:cc:dd:ee:ff", "mac must be preserved");
    }

    #[tokio::test]
    async fn test_validate_warmfork_snapshot_allows_network_devices() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("config.json");
        let cfg = serde_json::json!({
            "vsock": {"socket": "/old/task.vsock", "cid": 3},
            "net": [{"id": "tap0", "tap": "tap0"}]
        });
        tokio::fs::write(&src, serde_json::to_string_pretty(&cfg).unwrap())
            .await
            .unwrap();

        validate_snapshot_config(&src, &SnapshotType::WarmFork)
            .await
            .unwrap();
    }

    /// Mirrors the real CH state.json structure: device snapshots are object keys under
    /// snapshots["device-manager"]["snapshots"], and the device_tree lives inside a
    /// JSON-encoded string at snapshots["device-manager"]["snapshot_data"]["state"].
    #[tokio::test]
    async fn test_patch_snapshot_state_removes_net_and_pci_wrapper() {
        let dir = TempDir::new().unwrap();

        let device_tree_str = serde_json::to_string(&serde_json::json!({
            "device_tree": {
                "intf-2": {"id": "intf-2", "resources": [], "parent": "_virtio-pci-intf-2", "children": [], "pci_bdf": null},
                "_virtio-pci-intf-2": {"id": "_virtio-pci-intf-2", "resources": [], "parent": null, "children": ["intf-2"], "pci_bdf": "0000:00:02.0"},
                "vsock": {"id": "vsock", "resources": [], "parent": "_virtio-pci-vsock", "children": [], "pci_bdf": null},
                "_virtio-pci-vsock": {"id": "_virtio-pci-vsock", "resources": [], "parent": null, "children": ["vsock"], "pci_bdf": "0000:00:05.0"}
            }
        })).unwrap();

        let state = serde_json::json!({
            "snapshots": {
                "device-manager": {
                    "snapshots": {
                        "intf-2": {"snapshots": {}, "snapshot_data": {"state": "{\"avail_features\":0}"}},
                        "_virtio-pci-intf-2": {"snapshots": {}, "snapshot_data": {"state": "{\"pci\":1}"}},
                        "vsock": {"snapshots": {}, "snapshot_data": {"state": "{\"avail_features\":0}"}},
                        "_virtio-pci-vsock": {"snapshots": {}, "snapshot_data": {"state": "{\"pci\":2}"}}
                    },
                    "snapshot_data": {"state": device_tree_str}
                }
            },
            "snapshot_data": null
        });

        let state_path = dir.path().join("state.json");
        tokio::fs::write(&state_path, serde_json::to_string(&state).unwrap())
            .await
            .unwrap();

        patch_snapshot_state(dir.path()).await.unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&state_path).await.unwrap()).unwrap();

        let dm_snapshots = &patched["snapshots"]["device-manager"]["snapshots"];
        assert!(
            dm_snapshots.get("intf-2").is_none(),
            "intf-2 snapshot must be removed"
        );
        assert!(
            dm_snapshots.get("_virtio-pci-intf-2").is_none(),
            "_virtio-pci-intf-2 snapshot must be removed"
        );
        assert!(
            dm_snapshots.get("vsock").is_some(),
            "vsock snapshot must remain"
        );
        assert!(
            dm_snapshots.get("_virtio-pci-vsock").is_some(),
            "_virtio-pci-vsock snapshot must remain"
        );

        let inner_state_str = patched["snapshots"]["device-manager"]["snapshot_data"]["state"]
            .as_str()
            .unwrap();
        let inner: serde_json::Value = serde_json::from_str(inner_state_str).unwrap();
        let dt = &inner["device_tree"];
        assert!(
            dt.get("intf-2").is_none(),
            "intf-2 must be removed from device_tree"
        );
        assert!(
            dt.get("_virtio-pci-intf-2").is_none(),
            "_virtio-pci-intf-2 must be removed from device_tree"
        );
        assert!(
            dt.get("vsock").is_some(),
            "vsock must remain in device_tree"
        );
        assert!(
            dt.get("_virtio-pci-vsock").is_some(),
            "_virtio-pci-vsock must remain in device_tree"
        );
    }

    /// Regression test: state.json contains `intf-5` (template's link index) but the new
    /// sandbox's pending_net_hotplug would have provided `intf-2`.  The old ID-based approach
    /// would have missed `intf-5` entirely; the pattern-based scan must remove it regardless.
    #[tokio::test]
    async fn test_patch_snapshot_state_removes_mismatched_link_index() {
        let dir = TempDir::new().unwrap();

        let device_tree_str = serde_json::to_string(&serde_json::json!({
            "device_tree": {
                "intf-5": {"id": "intf-5", "resources": [], "parent": "_virtio-pci-intf-5", "children": [], "pci_bdf": null},
                "_virtio-pci-intf-5": {"id": "_virtio-pci-intf-5", "resources": [], "parent": null, "children": ["intf-5"], "pci_bdf": "0000:00:02.0"},
                "vsock": {"id": "vsock", "resources": [], "parent": "_virtio-pci-vsock", "children": [], "pci_bdf": null}
            }
        })).unwrap();

        let state = serde_json::json!({
            "snapshots": {
                "device-manager": {
                    "snapshots": {
                        "intf-5": {"snapshots": {}, "snapshot_data": {"state": "{\"avail_features\":0}"}},
                        "_virtio-pci-intf-5": {"snapshots": {}, "snapshot_data": {"state": "{\"pci\":1}"}},
                        "vsock": {"snapshots": {}, "snapshot_data": {"state": "{\"avail_features\":0}"}}
                    },
                    "snapshot_data": {"state": device_tree_str}
                }
            },
            "snapshot_data": null
        });

        let state_path = dir.path().join("state.json");
        tokio::fs::write(&state_path, serde_json::to_string(&state).unwrap())
            .await
            .unwrap();

        patch_snapshot_state(dir.path()).await.unwrap();

        let patched: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&state_path).await.unwrap()).unwrap();

        let dm_snapshots = &patched["snapshots"]["device-manager"]["snapshots"];
        assert!(
            dm_snapshots.get("intf-5").is_none(),
            "intf-5 snapshot must be removed"
        );
        assert!(
            dm_snapshots.get("_virtio-pci-intf-5").is_none(),
            "_virtio-pci-intf-5 snapshot must be removed"
        );
        assert!(
            dm_snapshots.get("vsock").is_some(),
            "vsock snapshot must remain"
        );

        let inner_state_str = patched["snapshots"]["device-manager"]["snapshot_data"]["state"]
            .as_str()
            .unwrap();
        let inner: serde_json::Value = serde_json::from_str(inner_state_str).unwrap();
        let dt = &inner["device_tree"];
        assert!(
            dt.get("intf-5").is_none(),
            "intf-5 must be removed from device_tree"
        );
        assert!(
            dt.get("_virtio-pci-intf-5").is_none(),
            "_virtio-pci-intf-5 must be removed from device_tree"
        );
        assert!(
            dt.get("vsock").is_some(),
            "vsock must remain in device_tree"
        );
    }
}
