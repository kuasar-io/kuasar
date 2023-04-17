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

use std::sync::Arc;

use anyhow::anyhow;
use containerd_sandbox::error::Result;
use futures_util::StreamExt;
use log::error;
use qapi::{
    futures::{QapiService, QmpStreamTokio},
    qmp::{device_del, Event, QmpCommand},
};
use tokio::{
    io::WriteHalf,
    net::UnixStream,
    sync::{
        oneshot::{channel, Sender},
        Mutex,
    },
    task::JoinHandle,
};

pub struct QmpClient {
    qmp: QapiService<QmpStreamTokio<WriteHalf<UnixStream>>>,
    watchers: Arc<Mutex<Vec<QmpEventWatcher>>>,
    #[allow(dead_code)]
    handle: JoinHandle<()>,
}

pub struct QmpEventWatcher {
    filter: Box<dyn Fn(&Event) -> bool + Sync + Send + 'static>,
    sender: Sender<Event>,
}

impl QmpClient {
    pub async fn new(socket_addr: &str) -> Result<Self> {
        let stream = qapi::futures::QmpStreamTokio::open_uds(socket_addr).await?;
        let stream = stream.negotiate().await?;
        let (service, mut events) = stream.into_parts();
        let event_watchers = Arc::new(Mutex::new(Vec::<QmpEventWatcher>::new()));

        let w_clone = event_watchers.clone();
        let handle = tokio::spawn(async move {
            while let Some(Ok(event)) = events.next().await {
                let mut ws = w_clone.lock().await;
                let mut retained = vec![];
                while let Some(w) = ws.pop() {
                    if (w.filter)(&event) {
                        match w.sender.send(event.clone()) {
                            Ok(_) => {}
                            Err(e) => {
                                error!("failed to send event to watcher {:?}", e)
                            }
                        }
                    } else {
                        retained.push(w);
                    }
                }
                *ws = retained;
            }
        });
        let client = Self {
            qmp: service,
            watchers: event_watchers,
            handle,
        };
        Ok(client)
    }

    pub async fn execute<C: QmpCommand + 'static>(&self, cmd: C) -> Result<C::Ok> {
        match self.qmp.execute(cmd).await {
            Ok(r) => Ok(r),
            Err(e) => Err(anyhow!("failed to execute qmp, {}", e).into()),
        }
    }

    pub async fn execute_and_wait_event<C: QmpCommand + 'static>(
        &self,
        cmd: C,
        filter: impl Fn(&Event) -> bool + Sync + Send + 'static,
    ) -> Result<C::Ok> {
        let (tx, rx) = channel();
        {
            let watcher = QmpEventWatcher {
                filter: Box::new(filter),
                sender: tx,
            };
            let mut watchers = self.watchers.lock().await;
            watchers.push(watcher);
        }
        match self.qmp.execute(cmd).await {
            Ok(r) => {
                //TODO  add timeout
                let _ = rx.await;
                Ok(r)
            }
            Err(e) => Err(anyhow!("failed to execute qmp, {}", e).into()),
        }
    }

    pub async fn delete_device(&self, device_id: &str) -> Result<()> {
        let device_id = device_id.to_string();
        self.execute_and_wait_event(
            device_del {
                id: device_id.clone(),
            },
            move |x| {
                if let qapi::qmp::Event::DEVICE_DELETED { ref data, .. } = x {
                    if let Some(id) = &data.device {
                        if id == &device_id {
                            return true;
                        }
                    }
                }
                false
            },
        )
        .await?;
        Ok(())
    }
}
