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

use std::ops::{Deref, DerefMut};

use containerd_sandbox::spec::Mount;
use serde::{Deserialize, Serialize};
use vmm_common::storage::Storage;

use super::extract_lower_dirs;

/// A read-only view of one device+mount entry in a [`DeviceGraph`].
///
/// Provides readable accessors for fields that `Storage` stores with
/// storage-oriented names, matching the names used in the design document.
#[allow(dead_code)]
pub(crate) struct DeviceGraphNode<'a> {
    storage: &'a Storage,
}

#[allow(dead_code)]
impl DeviceGraphNode<'_> {
    pub(crate) fn device_id(&self) -> Option<&str> {
        self.storage.device_id.as_deref()
    }

    /// Guest PCI address reported by the hypervisor hot-attach (BDF notation,
    /// e.g. `"0000:00:01.0"`).
    pub(crate) fn bdf(&self) -> &str {
        &self.storage.source
    }

    pub(crate) fn guest_mount(&self) -> &str {
        &self.storage.mount_point
    }

    /// Total number of container references held against this device.
    pub(crate) fn ref_count(&self) -> u32 {
        self.storage.ref_container.values().sum()
    }

    pub(crate) fn source_identity(&self) -> Option<&str> {
        self.storage.source_identity.as_deref()
    }

    /// Host-side path to the block image managed by the runtime, if any.
    pub(crate) fn artifact_path(&self) -> Option<&str> {
        self.storage.cleanup_path.as_deref()
    }

    pub(crate) fn is_runtime_owned(&self) -> bool {
        self.storage.owned_by_runtime
    }
}

/// Tracks block devices and their guest mounts for a single sandbox.
///
/// Serialises as a flat JSON array of `Storage` objects (via
/// `#[serde(transparent)]`) so existing sandbox dump files remain compatible.
/// All `Vec<Storage>` operations are available through `Deref`/`DerefMut`,
/// avoiding call-site churn.  Higher-level graph operations are exposed as
/// named methods.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct DeviceGraph {
    nodes: Vec<Storage>,
}

impl DeviceGraph {
    /// Return a named view of the node at `idx`.
    #[allow(dead_code)]
    pub(crate) fn node(&self, idx: usize) -> DeviceGraphNode<'_> {
        DeviceGraphNode {
            storage: &self.nodes[idx],
        }
    }

    /// Find the index of a reusable node whose source identity matches `mount`.
    pub(crate) fn find_reusable_index(
        &self,
        mount: &Mount,
        is_rootfs_mount: bool,
    ) -> Option<usize> {
        find_reusable_storage_index(&self.nodes, mount, is_rootfs_mount)
    }

    /// Find an orphan-container-owned node for `mount`.
    /// Returns `(container_id, storage_id)` when found.
    #[allow(dead_code)]
    pub(crate) fn find_orphan_for_mount(
        &self,
        orphan_container_ids: &[String],
        mount: &Mount,
        is_rootfs_mount: bool,
    ) -> Option<(String, String)> {
        find_orphan_for_mount(&self.nodes, orphan_container_ids, mount, is_rootfs_mount)
    }
}

impl From<Vec<Storage>> for DeviceGraph {
    fn from(nodes: Vec<Storage>) -> Self {
        Self { nodes }
    }
}

impl Deref for DeviceGraph {
    type Target = Vec<Storage>;
    fn deref(&self) -> &Vec<Storage> {
        &self.nodes
    }
}

impl DerefMut for DeviceGraph {
    fn deref_mut(&mut self) -> &mut Vec<Storage> {
        &mut self.nodes
    }
}

fn source_identity_for_mount(mount: &Mount) -> Option<String> {
    if let Some(lower_dirs) = extract_lower_dirs(&mount.options) {
        return Some(format!("overlay:{}", lower_dirs));
    }
    if !mount.source.is_empty() {
        return Some(format!("{}:{}", mount.r#type, mount.source));
    }
    None
}

fn storage_matches_mount(storage: &Storage, mount: &Mount, is_rootfs_mount: bool) -> bool {
    let req_identity = source_identity_for_mount(mount);
    if let (Some(storage_identity), Some(req_identity)) =
        (&storage.source_identity, req_identity.as_ref())
    {
        return storage_identity == req_identity && storage.r#type == mount.r#type;
    }

    if !is_rootfs_mount && storage.is_for_mount(mount) {
        return true;
    }

    let req_lower = extract_lower_dirs(&mount.options);
    if let (Some(storage_lower), Some(req_lower)) = (&storage.lower_dirs, req_lower) {
        return *storage_lower == req_lower && storage.r#type == mount.r#type;
    }

    false
}

fn find_reusable_storage_index(
    storages: &[Storage],
    mount: &Mount,
    is_rootfs_mount: bool,
) -> Option<usize> {
    storages
        .iter()
        .position(|storage| storage_matches_mount(storage, mount, is_rootfs_mount))
}

fn find_orphan_for_mount(
    storages: &[Storage],
    orphan_container_ids: &[String],
    mount: &Mount,
    is_rootfs_mount: bool,
) -> Option<(String, String)> {
    storages
        .iter()
        .filter(|storage| storage_matches_mount(storage, mount, is_rootfs_mount))
        .find_map(|storage| {
            storage.ref_container.keys().find_map(|container_id| {
                if orphan_container_ids.contains(container_id) {
                    Some((container_id.clone(), storage.id.clone()))
                } else {
                    None
                }
            })
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn storage(id: &str, host_source: &str, lower_dirs: Option<&str>) -> Storage {
        Storage {
            host_source: host_source.to_string(),
            r#type: "overlay".to_string(),
            id: id.to_string(),
            device_id: Some("blk1".to_string()),
            ref_container: HashMap::new(),
            need_guest_handle: true,
            source: "0000:00:01.0".to_string(),
            driver: "blk".to_string(),
            driver_options: vec![],
            fstype: "ext4".to_string(),
            options: vec![],
            mount_point: "/run/kuasar/storage/containers/storage1".to_string(),
            lower_dirs: lower_dirs.map(str::to_string),
            cleanup_path: None,
            owned_by_runtime: false,
            source_identity: lower_dirs.map(|lower| format!("overlay:{}", lower)),
        }
    }

    fn overlay_mount(source: &str, lower_dirs: Option<&str>) -> Mount {
        let mut options = vec![];
        if let Some(lower_dirs) = lower_dirs {
            options.push(format!("lowerdir={}", lower_dirs));
        }
        Mount {
            r#type: "overlay".to_string(),
            source: source.to_string(),
            destination: "/".to_string(),
            options,
        }
    }

    #[test]
    fn test_rootfs_does_not_match_by_empty_host_source() {
        let storage = storage("storage1", "", Some("/layers/a:/layers/b"));
        let mount = overlay_mount("", None);

        assert!(!storage_matches_mount(&storage, &mount, true));
    }

    #[test]
    fn test_rootfs_matches_by_lower_dirs() {
        let storage = storage("storage1", "", Some("/layers/a:/layers/b"));
        let mount = overlay_mount("", Some("/layers/a:/layers/b"));

        assert!(storage_matches_mount(&storage, &mount, true));
    }

    #[test]
    fn test_rootfs_matches_by_source_identity() {
        let mut storage = storage("storage1", "", None);
        storage.source_identity = Some("overlay:/layers/a:/layers/b".to_string());
        let mount = overlay_mount("", Some("/layers/a:/layers/b"));

        assert!(storage_matches_mount(&storage, &mount, true));
    }

    #[test]
    fn test_bind_matches_by_source_identity() {
        let mut storage = storage("storage1", "/old/path", None);
        storage.r#type = "bind".to_string();
        storage.source_identity = Some("bind:/host/path".to_string());
        let mount = Mount {
            r#type: "bind".to_string(),
            source: "/host/path".to_string(),
            destination: "/data".to_string(),
            options: vec![],
        };

        assert!(storage_matches_mount(&storage, &mount, false));
    }

    #[test]
    fn test_non_rootfs_matches_by_host_source() {
        let storage = storage("storage1", "/host/path", None);
        let mount = overlay_mount("/host/path", None);

        assert!(storage_matches_mount(&storage, &mount, false));
    }

    #[test]
    fn test_find_orphan_for_mount() {
        let mut storage = storage("storage1", "/host/path", None);
        storage
            .ref_container
            .insert("orphan-container".to_string(), 1);
        let mount = overlay_mount("/host/path", None);

        assert_eq!(
            find_orphan_for_mount(&[storage], &["orphan-container".to_string()], &mount, false),
            Some(("orphan-container".to_string(), "storage1".to_string()))
        );
    }

    #[test]
    fn device_graph_node_accessors_map_to_storage_fields() {
        let mut s = storage("node1", "/host/path", Some("/layers/a:/layers/b"));
        s.device_id = Some("blk-0".to_string());
        s.source = "0000:00:02.0".to_string();
        s.mount_point = "/run/kuasar/storage/containers/node1".to_string();
        s.cleanup_path = Some("/sandbox/node1.img".to_string());
        s.owned_by_runtime = true;
        s.ref_container.insert("container-a".to_string(), 1);
        s.ref_container.insert("container-b".to_string(), 2);

        let graph = DeviceGraph::from(vec![s]);
        let node = graph.node(0);

        assert_eq!(node.device_id(), Some("blk-0"));
        assert_eq!(node.bdf(), "0000:00:02.0");
        assert_eq!(node.guest_mount(), "/run/kuasar/storage/containers/node1");
        assert_eq!(node.ref_count(), 3); // 1 + 2
        assert_eq!(node.source_identity(), Some("overlay:/layers/a:/layers/b"));
        assert_eq!(node.artifact_path(), Some("/sandbox/node1.img"));
        assert!(node.is_runtime_owned());
    }

    #[test]
    fn device_graph_find_reusable_index_delegates_to_storage_matches() {
        let s = storage("storage1", "", Some("/layers/a:/layers/b"));
        let graph = DeviceGraph::from(vec![s]);
        let mount = overlay_mount("", Some("/layers/a:/layers/b"));

        assert_eq!(graph.find_reusable_index(&mount, true), Some(0));
        assert_eq!(graph.find_reusable_index(&mount, false), Some(0));
    }

    #[test]
    fn device_graph_serialises_as_flat_array() {
        let s = storage("storage1", "/host/path", None);
        let graph = DeviceGraph::from(vec![s]);

        let json = serde_json::to_string(&graph).unwrap();
        // Must be a JSON array, not an object with a "nodes" key
        assert!(json.starts_with('['), "expected flat array, got: {}", json);

        let roundtrip: DeviceGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.len(), 1);
        assert_eq!(roundtrip[0].id, "storage1");
    }
}
