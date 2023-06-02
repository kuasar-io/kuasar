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

use futures_util::StreamExt;
use netlink_packet_core::{NetlinkMessage, NLM_F_ACK, NLM_F_CREATE, NLM_F_EXCL, NLM_F_REQUEST};
use netlink_packet_route::{RtnlMessage, TcMessage};
use rtnetlink::{try_nl, Error, Handle};

#[allow(dead_code)]
const HANDLE_INGRESS: u32 = 0xfffffff1;
#[allow(dead_code)]
const HANDLE_TC_FILTER: u32 = 0xffff0000;

pub struct QDiscAddRequest {
    handle: Handle,
    message: TcMessage,
}

#[allow(dead_code)]
impl QDiscAddRequest {
    pub(crate) fn new(handle: Handle) -> Self {
        QDiscAddRequest {
            handle,
            message: TcMessage::default(),
        }
    }

    pub async fn execute(self) -> Result<(), Error> {
        let QDiscAddRequest {
            mut handle,
            message,
        } = self;

        let mut req = NetlinkMessage::from(RtnlMessage::NewQueueDiscipline(message));
        req.header.flags = NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK;

        let mut response = handle.request(req)?;
        while let Some(message) = response.next().await {
            try_nl!(message);
        }
        Ok(())
    }

    pub fn if_index(mut self, if_index: i32) -> Self {
        self.message.header.index = if_index;
        self
    }

    pub fn ingress(mut self) -> Self {
        self.message.header.parent = HANDLE_INGRESS;
        self
    }
}

#[allow(dead_code)]
pub struct TrafficFilterSetRequest {
    handle: Handle,
    message: TcMessage,
}

#[allow(dead_code)]
impl TrafficFilterSetRequest {
    pub(crate) fn new(handle: Handle, ifindex: i32) -> Self {
        let mut message = TcMessage::default();
        message.header.index = ifindex;
        message.header.parent = HANDLE_TC_FILTER;

        Self { handle, message }
    }

    pub async fn execute(self) -> Result<(), Error> {
        let TrafficFilterSetRequest {
            mut handle,
            message,
        } = self;

        let mut req = NetlinkMessage::from(RtnlMessage::NewTrafficFilter(message));
        req.header.flags = NLM_F_REQUEST | NLM_F_CREATE | NLM_F_EXCL | NLM_F_ACK;

        let mut response = handle.request(req)?;
        while let Some(message) = response.next().await {
            try_nl!(message);
        }
        Ok(())
    }

    pub fn mirror(self, _dest_index: i32) -> Self {
        //TODO
        self
    }
}
