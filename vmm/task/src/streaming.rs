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

use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use containerd_shim::protos::protobuf::{CodedInputStream, Message};
use futures::{ready, Future};
use lazy_static::lazy_static;
use log::{debug, info, warn};
use protobuf::MessageFull;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    pin, select,
    sync::{
        mpsc::{channel, error::SendError, OwnedPermit, Receiver, Sender},
        Mutex, Notify,
    },
};
use ttrpc::{asynchronous::ServerStream, r#async::TtrpcContext};
use vmm_common::{
    api,
    api::{
        any::Any,
        data::{Data, WindowUpdate},
        empty::Empty,
        streaming::StreamInit,
    },
};

macro_rules! new_any {
    ($ty:ty, $value:expr) => {{
        let mut a = vmm_common::api::any::Any::new();
        a.type_url = <$ty>::descriptor().full_name().to_string();
        a.value = $value;
        a
    }};
    ($ty:ty) => {{
        let mut a = vmm_common::api::any::Any::new();
        a.type_url = <$ty>::descriptor().full_name().to_string();
        a.value = <$ty>::new().write_to_bytes().unwrap_or_default();
        a
    }};
}

lazy_static! {
    pub static ref STREAMING_SERVICE: Service = Service {
        ios: Arc::new(Mutex::new(HashMap::default()))
    };
}

const WINDOW_SIZE: i32 = 32 * 1024;

#[derive(Clone)]
pub struct Service {
    ios: Arc<Mutex<HashMap<String, IOChannel>>>,
}

pub struct IOChannel {
    sender: Option<Sender<Vec<u8>>>,
    receiver: Option<Receiver<Vec<u8>>>,
    remaining_data: Option<Any>,
    preemption_sender: Option<Sender<()>>,
    notifier: Arc<Notify>,
    sender_closed: bool,
}

pub struct PreemptableReceiver {
    receiver: Receiver<Vec<u8>>,
    preempt: Receiver<()>,
}

impl PreemptableReceiver {
    pub fn new(rx: Receiver<Vec<u8>>, preempt_rx: Receiver<()>) -> Self {
        Self {
            receiver: rx,
            preempt: preempt_rx,
        }
    }

    pub async fn recv(&mut self) -> ttrpc::Result<Option<Vec<u8>>> {
        select! {
            res = self.receiver.recv() => {
                Ok(res)
            }
            _ = self.preempt.recv() => {
                Err(ttrpc::Error::Others("channel is preempted".to_string()))
            }
        }
    }
}

impl IOChannel {
    pub fn new() -> Self {
        let (tx, rx) = channel(128);
        Self {
            sender: Some(tx),
            receiver: Some(rx),
            remaining_data: None,
            preemption_sender: None,
            notifier: Arc::new(Notify::new()),
            sender_closed: false,
        }
    }

    async fn get_or_preempt_receiver(&mut self) -> Option<PreemptableReceiver> {
        if let Some(r) = self.receiver.take() {
            let (tx, rx) = channel(1);
            let preempt_receiver = PreemptableReceiver::new(r, rx);
            self.preemption_sender = Some(tx);
            return Some(preempt_receiver);
        }
        if let Some(r) = self.preemption_sender.take() {
            debug!("send preemption message");
            if let Err(e) = r.send(()).await {
                warn!("failed to send preemption message: {}", e);
            }
        }
        None
    }

    fn return_preempted_receiver(
        &mut self,
        r: PreemptableReceiver,
        remaining_data: Option<Any>,
    ) -> bool {
        // only return the receiver when sender is already moved to the io thread, and sender is not closed
        if self.sender.is_none() && !self.sender_closed {
            self.receiver = Some(r.receiver);
            self.remaining_data = remaining_data;
            self.notifier.notify_one();
            return true;
        }
        false
    }
}

#[async_trait]
impl api::streaming_ttrpc::Streaming for Service {
    async fn stream(
        &self,
        _ctx: &TtrpcContext,
        mut stream: ServerStream<Any, Any>,
    ) -> ::ttrpc::Result<()> {
        let stream_id = if let Some(i) = stream.recv().await? {
            let mut stream_init = StreamInit::new();
            let mut input = CodedInputStream::from_bytes(i.value.as_slice());
            stream_init
                .merge_from(&mut input)
                .map_err(ttrpc::err_to_others!(e, "failed to unmarshal StreamInit"))?;
            stream_init.id
        } else {
            return Err(ttrpc::Error::Others(
                "can not receive streamInit".to_string(),
            ));
        };
        debug!("handle stream with id {}", stream_id);
        let a = new_any!(Empty);
        stream.send(&a).await?;

        if stream_id.ends_with("stdin") {
            self.handle_stdin(&stream_id, stream).await?;
        } else if stream_id.ends_with("stdout") || stream_id.ends_with("stderr") {
            self.handle_stdout(&stream_id, stream).await?;
        } else {
            warn!("unrecognized stream {}", stream_id);
        }

        debug!("stream with id {} handle finished", stream_id);
        Ok(())
    }
}

impl Service {
    async fn get_or_insert_sender(&self, id: &str) -> ttrpc::Result<Sender<Vec<u8>>> {
        let mut ios = self.ios.lock().await;
        let ch = ios.entry(id.to_string()).or_insert(IOChannel::new());
        ch.sender.take().ok_or(ttrpc::Error::Others(
            "someone is taking the channel sender".to_string(),
        ))
    }

    async fn preempt_receiver(&self, id: &str) -> ttrpc::Result<PreemptableReceiver> {
        for _i in 0..10 {
            let mut ios = self.ios.lock().await;
            let ch = ios.entry(id.to_string()).or_insert(IOChannel::new());
            let notifier = ch.notifier.clone();
            if let Some(c) = ch.get_or_preempt_receiver().await {
                debug!("io channel {} being preempted", id);
                return Ok(c);
            }
            // Release the lock here so that they can get the lock when return_prempted_receiver
            drop(ios);

            notifier.notified().await;
        }

        Err(ttrpc::Error::Others(
            "failed to preempt io channel".to_string(),
        ))
    }

    async fn return_preempted_receiver(
        &self,
        id: &str,
        r: PreemptableReceiver,
        remaining_data: Option<Any>,
    ) {
        let mut ios = self.ios.lock().await;
        let mut channel_used = false;
        if let Some(ch) = ios.get_mut(id) {
            if ch.return_preempted_receiver(r, remaining_data) {
                channel_used = true;
            }
        } else {
            warn!("io channel removed when return the receiver");
        }
        if !channel_used {
            self.ios.lock().await.remove(id);
        }
    }

    async fn get_remaining_data(&self, id: &str) -> Option<Any> {
        self.ios
            .lock()
            .await
            .get_mut(id)
            .and_then(|x| x.remaining_data.take())
    }

    pub async fn get_stdin(&self, id: &str) -> containerd_shim::Result<StreamingStdin> {
        self.ios
            .lock()
            .await
            .get_mut(id)
            .ok_or(containerd_shim::Error::NotFoundError(
                "can not get stdin stream".to_string(),
            ))?
            .receiver
            .take()
            .map(|r| StreamingStdin { receiver: r })
            .ok_or(containerd_shim::Error::Other(
                "someone is taking the io channel".to_string(),
            ))
    }

    pub async fn get_output(&self, id: &str) -> containerd_shim::Result<StreamingOutput> {
        self.ios
            .lock()
            .await
            .get_mut(id)
            .ok_or(containerd_shim::Error::NotFoundError(
                "can not get output stream".to_string(),
            ))?
            .sender
            .take()
            .map(|s| StreamingOutput {
                sender: s,
                permit: None,
            })
            .ok_or(containerd_shim::Error::Other(
                "someone is taking the io channel".to_string(),
            ))
    }

    async fn remove_io_channel(&self, id: &str) {
        self.ios.lock().await.remove(id);
    }

    async fn handle_stdin(
        &self,
        stream_id: &String,
        mut stream: ServerStream<Any, Any>,
    ) -> ttrpc::Result<()> {
        let sender = self.get_or_insert_sender(stream_id).await?;
        let mut window = 0i32;
        loop {
            if window < WINDOW_SIZE {
                let mut update = WindowUpdate::new();
                update.update = WINDOW_SIZE;
                let update_bytes = match update.write_to_bytes() {
                    Ok(d) => d,
                    Err(e) => {
                        debug!("failed to marshal update of stream {}, {}", stream_id, e);
                        self.ios.lock().await.remove(stream_id);
                        return Err(ttrpc::Error::Others(format!("failed to write data {}", e)));
                    }
                };
                let a = new_any!(WindowUpdate, update_bytes);
                if let Err(e) = stream.send(&a).await {
                    debug!("failed to send update of stream {}, {}", stream_id, e);
                    self.ios.lock().await.remove(stream_id);
                    return Err(e);
                }
                window += WINDOW_SIZE;
            }
            match stream.recv().await? {
                Some(d) => {
                    let data_bytes = {
                        let mut data = Data::new();
                        let mut input = CodedInputStream::from_bytes(d.value.as_slice());
                        data.merge_from(&mut input)
                            .map_err(ttrpc::err_to_others!(e, "data format error"))?;
                        data.data
                    };
                    let len: i32 = data_bytes.len().try_into().unwrap_or_default();
                    if let Err(e) = sender.send(data_bytes).await {
                        self.ios.lock().await.remove(stream_id);
                        return Err(ttrpc::Error::Others(format!("failed to send data {}", e)));
                    }
                    window -= len;
                }
                None => {
                    self.ios.lock().await.remove(stream_id);
                    return Ok(());
                }
            }
        }
    }

    async fn handle_stdout(
        &self,
        stream_id: &String,
        stream: ServerStream<Any, Any>,
    ) -> ttrpc::Result<()> {
        // TODO the stream needs to return if client send close.
        let mut receiver = self.preempt_receiver(stream_id).await?;
        if let Some(a) = self.get_remaining_data(stream_id).await {
            if let Err(e) = stream.send(&a).await {
                debug!("failed to send data of stream {}, {}", stream_id, e);
                self.return_preempted_receiver(stream_id, receiver, Some(a))
                    .await;
                return Err(e);
            }
        }
        loop {
            let r = if let Ok(res) = receiver.recv().await {
                res
            } else {
                self.return_preempted_receiver(stream_id, receiver, None)
                    .await;
                info!("stream {} is preempted", stream_id);
                return Err(ttrpc::Error::Others("channel is preempted".to_string()));
            };
            match r {
                Some(d) => {
                    if d.is_empty() {
                        self.ios.lock().await.remove(stream_id);
                        return Ok(());
                    }
                    let mut data = Data::new();
                    data.data = d;
                    let data_bytes = match data.write_to_bytes() {
                        Ok(b) => b,
                        Err(e) => {
                            debug!("failed to marshal data of stream {}, {}", stream_id, e);
                            self.return_preempted_receiver(stream_id, receiver, None)
                                .await;
                            return Err(ttrpc::Error::Others(format!(
                                "failed to write data {}",
                                e
                            )));
                        }
                    };
                    let a = new_any!(Data, data_bytes);
                    match stream.send(&a).await {
                        Ok(_) => {}
                        Err(e) => {
                            debug!("failed to send data of stream {}, {}", stream_id, e);
                            self.return_preempted_receiver(stream_id, receiver, Some(a))
                                .await;
                            return Err(e);
                        }
                    };
                }
                None => {
                    self.ios.lock().await.remove(stream_id);
                    return Ok(());
                }
            }
        }
    }
}

pub async fn get_stdin(url: &str) -> containerd_shim::Result<StreamingStdin> {
    let id = get_id(url)?;
    STREAMING_SERVICE.get_stdin(id).await
}

pub async fn remove_channel(url: &str) -> containerd_shim::Result<()> {
    let id = get_id(url)?;
    STREAMING_SERVICE.remove_io_channel(id).await;
    Ok(())
}

pub async fn close_stdout(url: &str) -> containerd_shim::Result<()> {
    let id = get_id(url)?;
    if let Some(ch) = STREAMING_SERVICE.ios.lock().await.get_mut(id) {
        ch.sender_closed = true;
    }
    Ok(())
}

pub async fn get_output(url: &str) -> containerd_shim::Result<StreamingOutput> {
    let id = get_id(url)?;
    STREAMING_SERVICE.get_output(id).await
}

// url of the streaming should be in the form of
// ttrpc+hvsock://aaa/bbb?id=<container-id>-stdin
// get_id get the <container-id>-stdin out of it.
fn get_id(url: &str) -> containerd_shim::Result<&str> {
    let id_parts = url.split("id=").collect::<Vec<&str>>();
    if id_parts.len() != 2 {
        return Err(containerd_shim::Error::InvalidArgument(
            "streaming url invalid, no id".to_string(),
        ));
    }
    Ok(id_parts[1].trim_matches('&'))
}

pin_project_lite::pin_project! {
    pub struct StreamingStdin {
        receiver: Receiver<Vec<u8>>
    }
}

impl AsyncRead for StreamingStdin {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.project();
        let r = ready!(this.receiver.poll_recv(cx));
        match r {
            Some(a) => {
                buf.put_slice(a.as_slice());
                Poll::Ready(Ok(()))
            }
            None => Poll::Ready(Ok(())),
        }
    }
}

type Permit = Box<dyn Future<Output = Result<OwnedPermit<Vec<u8>>, SendError<()>>> + Send>;

pin_project_lite::pin_project! {
    pub struct StreamingOutput {
        sender: Sender<Vec<u8>>,
        permit: Option<Pin<Permit>>,
    }
}

impl AsyncWrite for StreamingOutput {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.project();
        let permit_fut = this.permit.get_or_insert_with(|| {
            let permit_fut = this.sender.clone().reserve_owned();
            Box::pin(permit_fut)
        });
        pin!(permit_fut);
        let permit = ready!(permit_fut.poll(cx));
        match permit {
            Ok(p) => {
                p.send(buf.to_vec());
                *this.permit = None;
                Poll::Ready(Ok(buf.len()))
            }
            Err(e) => {
                *this.permit = None;
                Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}
