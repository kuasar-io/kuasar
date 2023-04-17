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

use std::ptr::NonNull;

use async_trait::async_trait;
use containerd_sandbox::{error::Result, ContainerOption, Sandbox};
use log::warn;

use crate::{
    container::handler::{
        append::MetadataAddHandler,
        io::IoHandler,
        mount::MountHandler,
        ns::NamespaceHandler,
        process::{ProcessHandler, ProcessRemoveHandler},
        spec::SpecHandler,
        storage::StorageHandler,
    },
    vm::VM,
    KuasarSandbox,
};

pub mod append;
mod io;
mod mount;
mod ns;
mod process;
mod spec;
mod storage;

pub struct HandlerNode<S> {
    pub handler: Box<dyn Handler<S> + Sync + Send>,
    pub next: Option<NonNull<HandlerNode<S>>>,
    pub prev: Option<NonNull<HandlerNode<S>>>,
}

unsafe impl<S> Sync for HandlerNode<S> {}

unsafe impl<S> Send for HandlerNode<S> {}

impl<S> HandlerNode<S>
where
    S: Sync + Send,
{
    #[allow(dead_code)]
    pub fn new(handler: Box<dyn Handler<S> + Sync + Send>) -> Self {
        Self {
            handler,
            next: None,
            prev: None,
        }
    }

    #[allow(dead_code)]
    pub async fn handle(&self, sandbox: &mut S) -> Result<()> {
        self.handler.handle(sandbox).await
    }

    #[allow(dead_code)]
    pub async fn rollback(&self, sandbox: &mut S) -> Result<()> {
        self.handler.rollback(sandbox).await
    }
}

pub struct HandlerChain<S> {
    handlers: Vec<Box<dyn Handler<S> + Sync + Send>>,
}

impl<S> HandlerChain<S>
where
    S: Sync + Send,
{
    pub fn from(handlers: Vec<Box<dyn Handler<S> + Sync + Send>>) -> Self {
        Self { handlers }
    }

    pub async fn handle(&self, sandbox: &mut S) -> Result<()> {
        let mut finished_index = 0;
        let mut err = None;
        for (i, handler) in self.handlers.iter().enumerate() {
            match handler.handle(sandbox).await {
                Ok(_) => {
                    finished_index = i;
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let len = self.handlers.len();
        match err {
            None => {}
            Some(e) => {
                for (_, handler) in self
                    .handlers
                    .iter()
                    .rev()
                    .enumerate()
                    .filter(|(i, _)| *i >= len - 1 - finished_index)
                {
                    handler.rollback(sandbox).await.unwrap_or_else(|e| {
                        warn!("rollback failed for sandbox {:?}", e);
                    });
                }
                return Err(e);
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn rollback(&self, sandbox: &mut S) -> Result<()> {
        for handler in self.handlers.iter().rev() {
            handler.rollback(sandbox).await.unwrap_or_else(|e| {
                warn!("rollback failed for sandbox {:?}", e);
            });
        }
        Ok(())
    }
}

impl<V> KuasarSandbox<V>
where
    V: VM + Sync + Send,
{
    pub fn container_append_handlers(
        &self,
        id: &str,
        options: ContainerOption,
    ) -> Result<HandlerChain<Self>> {
        let rootfs = options.container.rootfs.clone();
        let mounts = if let Some(spec) = &options.container.spec {
            spec.mounts.clone()
        } else {
            vec![]
        };
        let io = options.container.io.clone();
        let mut handlers: Vec<Box<dyn Handler<Self> + Sync + Send>> = vec![];
        let metadata_handler = MetadataAddHandler::new(id, options);
        handlers.push(Box::new(metadata_handler));
        let ns_handler = NamespaceHandler::new(id);
        handlers.push(Box::new(ns_handler));
        for m in rootfs {
            let mh = MountHandler::new(id, m);
            handlers.push(Box::new(mh));
        }
        for m in mounts {
            let mh = MountHandler::new(id, m);
            handlers.push(Box::new(mh));
        }
        let storage_handler = StorageHandler::new(id);
        handlers.push(Box::new(storage_handler));

        if let Some(io) = io {
            let io_handler = IoHandler::new(id, io);
            handlers.push(Box::new(io_handler));
        }
        let spec_handler = SpecHandler::new(id);
        handlers.push(Box::new(spec_handler));

        Ok(HandlerChain::from(handlers))
    }

    pub async fn container_update_handlers(
        &self,
        id: &str,
        options: ContainerOption,
    ) -> Result<HandlerChain<Self>> {
        let mut handlers: Vec<Box<dyn Handler<Self> + Sync + Send>> = vec![];
        let container = self.container(id).await?;
        for kp in &container.processes {
            let mut exist = false;
            for p in &options.container.processes {
                if kp.id == p.id {
                    exist = true;
                }
            }
            if !exist {
                handlers.push(Box::new(ProcessRemoveHandler::new(id, &kp.id)));
            }
        }
        for proc in options.container.processes {
            let mut exist = false;
            for p in &container.processes {
                if p.id == proc.id {
                    exist = true;
                }
            }
            if !exist {
                handlers.push(Box::new(ProcessHandler::new(id, proc)));
            }
        }
        Ok(HandlerChain::from(handlers))
    }
}

#[async_trait]
pub trait Handler<T> {
    async fn handle(&self, sandbox: &mut T) -> Result<()>;
    async fn rollback(&self, sandbox: &mut T) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use async_trait::async_trait;

    use crate::container::handler::{Handler, HandlerChain};

    pub struct Sandbox {
        tests: Vec<i32>,
    }

    pub struct MockSucceedHandler {
        test: i32,
    }

    pub struct MockFailHandler {}

    #[async_trait]
    impl Handler<Sandbox> for MockSucceedHandler {
        async fn handle(&self, sandbox: &mut Sandbox) -> containerd_sandbox::error::Result<()> {
            sandbox.tests.push(self.test);
            Ok(())
        }

        async fn rollback(&self, sandbox: &mut Sandbox) -> containerd_sandbox::error::Result<()> {
            sandbox.tests.retain(|&x| x != self.test);
            Ok(())
        }
    }

    #[async_trait]
    impl Handler<Sandbox> for MockFailHandler {
        async fn handle(&self, _sandbox: &mut Sandbox) -> containerd_sandbox::error::Result<()> {
            Err(anyhow!("handle failed").into())
        }

        async fn rollback(&self, _sandbox: &mut Sandbox) -> containerd_sandbox::error::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_succeed() {
        let mut handlers: Vec<Box<dyn Handler<Sandbox> + Sync + Send>> = vec![];
        for i in 0..5 {
            handlers.push(Box::new(MockSucceedHandler { test: i }));
        }
        let handlers = HandlerChain::from(handlers);
        let mut s = Sandbox { tests: vec![] };
        handlers.handle(&mut s).await.unwrap();
        assert_eq!(s.tests, &[0, 1, 2, 3, 4]);
        handlers.rollback(&mut s).await.unwrap();
        assert!(s.tests.is_empty());
    }

    #[tokio::test]
    async fn test_fail() {
        let mut handlers: Vec<Box<dyn Handler<Sandbox> + Sync + Send>> = vec![];
        for i in 0..5 {
            handlers.push(Box::new(MockSucceedHandler { test: i }));
        }
        handlers.push(Box::new(MockFailHandler {}));
        let handlers = HandlerChain::from(handlers);
        let mut s = Sandbox { tests: vec![] };
        let res = handlers.handle(&mut s).await;
        assert!(res.is_err());
    }
}
