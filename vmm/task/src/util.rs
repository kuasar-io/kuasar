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

use std::path::Path;

use containerd_shim::{
    asynchronous::{
        monitor::{monitor_unsubscribe, Subscription},
        util::read_file_to_str,
    },
    error::{Error, Result},
    monitor::{ExitEvent, Subject},
    other_error,
};
use vmm_common::{storage::Storage, Io, IO_FILE_PREFIX, STORAGE_FILE_PREFIX};

pub async fn wait_pid(pid: i32, s: Subscription) -> i32 {
    let mut s = s;
    loop {
        if let Some(ExitEvent {
            subject: Subject::Pid(epid),
            exit_code: code,
        }) = s.rx.recv().await
        {
            if pid == epid {
                monitor_unsubscribe(s.id).await.unwrap_or_default();
                return code;
            }
        }
    }
}

pub async fn read_storages(bundle: impl AsRef<Path>, id: &str) -> Result<Vec<Storage>> {
    let storage_file_name = format!("{}-{}", STORAGE_FILE_PREFIX, id);
    let path = bundle.as_ref().join(&storage_file_name);
    let content = read_file_to_str(&path).await?;
    serde_json::from_str::<Vec<Storage>>(content.as_str()).map_err(other_error!(e, "read storage"))
}

pub async fn read_io(
    bundle: impl AsRef<Path>,
    container_id: &str,
    exec_id: Option<&str>,
) -> Result<Io> {
    let io_file_name = if let Some(eid) = exec_id {
        format!("{}-{}-{}", IO_FILE_PREFIX, container_id, eid)
    } else {
        format!("{}-{}", IO_FILE_PREFIX, container_id)
    };
    let path = bundle.as_ref().join(&io_file_name);
    let content = read_file_to_str(&path).await?;
    serde_json::from_str::<Io>(content.as_str()).map_err(other_error!(e, "read io"))
}
