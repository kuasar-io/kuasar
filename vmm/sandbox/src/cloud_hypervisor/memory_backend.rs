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

use std::path::PathBuf;

use containerd_sandbox::error::Result;

use crate::{sandbox::MemoryRestoreMode, vm::RestoreSource};

#[derive(Debug)]
pub(crate) struct PreparedMemoryBackend {
    pub(crate) source_url: String,
    pub(crate) mode: MemoryRestoreMode,
    #[allow(dead_code)]
    pinned_paths: Vec<PathBuf>,
}

pub(crate) fn memory_artifacts_for_mode(mode: &MemoryRestoreMode) -> Vec<&'static str> {
    let mut artifacts = vec!["memory-ranges", "state.json"];
    match mode {
        MemoryRestoreMode::Copy | MemoryRestoreMode::OnDemand => {}
        MemoryRestoreMode::FileBackend => artifacts.push("memory.img"),
        MemoryRestoreMode::ExternalUffd => {
            artifacts.push("memory.img");
            artifacts.push("memory-ranges.manifest");
        }
    }
    artifacts
}

pub(crate) async fn prepare_memory_backend(src: &RestoreSource) -> Result<PreparedMemoryBackend> {
    let artifacts = memory_artifacts_for_mode(&src.memory_restore_mode);
    for artifact in &artifacts {
        let source = src.snapshot_dir.join(artifact);
        tokio::fs::metadata(&source)
            .await
            .map_err(|e| anyhow::anyhow!("memory artifact {} is not available: {}", artifact, e))?;
        super::symlink_idempotent(source, src.work_dir.join(artifact), artifact).await?;
    }

    Ok(PreparedMemoryBackend {
        source_url: format!("file://{}", src.work_dir.display()),
        mode: src.memory_restore_mode.clone(),
        pinned_paths: artifacts
            .iter()
            .map(|artifact| src.work_dir.join(artifact))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use temp_dir::TempDir;

    use super::*;
    use crate::{
        sandbox::{MemoryRestoreMode, TemplateLeaseMode},
        template::SnapshotType,
        vm::SnapshotPathOverrides,
    };

    #[test]
    fn test_memory_artifacts_for_restore_modes() {
        assert_eq!(
            memory_artifacts_for_mode(&MemoryRestoreMode::Copy),
            vec!["memory-ranges", "state.json"]
        );
        assert_eq!(
            memory_artifacts_for_mode(&MemoryRestoreMode::OnDemand),
            vec!["memory-ranges", "state.json"]
        );
        assert_eq!(
            memory_artifacts_for_mode(&MemoryRestoreMode::FileBackend),
            vec!["memory-ranges", "state.json", "memory.img"]
        );
        assert_eq!(
            memory_artifacts_for_mode(&MemoryRestoreMode::ExternalUffd),
            vec![
                "memory-ranges",
                "state.json",
                "memory.img",
                "memory-ranges.manifest"
            ]
        );
    }

    fn restore_source(dir: &TempDir, mode: MemoryRestoreMode) -> RestoreSource {
        RestoreSource {
            snapshot_dir: dir.path().join("snapshot"),
            work_dir: dir.path().join("work"),
            overrides: SnapshotPathOverrides {
                task_vsock: "/tmp/task.vsock".to_string(),
                console_path: "/tmp/console.log".to_string(),
            },
            snapshot_type: SnapshotType::Environment,
            disk_images: vec![],
            storages: vec![],
            orphan_container_ids: vec![],
            memory_restore_mode: mode,
            reflink_supported: false,
            lease_mode: TemplateLeaseMode::Shared,
            warm_fork_params: None,
        }
    }

    #[tokio::test]
    async fn prepare_memory_backend_requires_mode_artifacts() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path().join("snapshot"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("work"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/memory-ranges"), b"ranges")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/state.json"), b"{}")
            .await
            .unwrap();

        let src = restore_source(&dir, MemoryRestoreMode::FileBackend);
        let result = prepare_memory_backend(&src).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("memory.img"));
    }

    #[tokio::test]
    async fn prepare_memory_backend_pins_copy_artifacts() {
        let dir = TempDir::new().unwrap();
        tokio::fs::create_dir_all(dir.path().join("snapshot"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("work"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/memory-ranges"), b"ranges")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("snapshot/state.json"), b"{}")
            .await
            .unwrap();

        let src = restore_source(&dir, MemoryRestoreMode::Copy);
        let prepared = prepare_memory_backend(&src).await.unwrap();

        assert_eq!(prepared.mode, MemoryRestoreMode::Copy);
        assert_eq!(
            prepared.pinned_paths,
            vec![
                PathBuf::from(dir.path().join("work/memory-ranges")),
                PathBuf::from(dir.path().join("work/state.json")),
            ]
        );
        assert!(
            tokio::fs::symlink_metadata(dir.path().join("work/memory-ranges"))
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
