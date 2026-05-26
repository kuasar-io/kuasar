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

//! Client library for the vmm-sandboxer Unix-socket service API.
//!
//! All VM operations are delegated to the running sandboxer daemon via a
//! newline-delimited JSON protocol.  This crate provides two typed API objects:
//!
//! - [`template::TemplateApi`] — template pool operations (create, status, refill, GC)
//! - [`sandbox::SandboxApi`]  — sandbox lifecycle operations (run, destroy)
//!
//! # Example
//! ```no_run
//! use std::path::Path;
//! use vmm_client::template::TemplateApi;
//! use vmm_client::sandbox::SandboxApi;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let sock = Path::new("/run/kuasar-vmm-admin.sock");
//!
//! let status = TemplateApi::new(sock).pool_status().await?;
//! println!("{}", serde_json::to_string_pretty(&status.0)?);
//!
//! let result = SandboxApi::new(sock)
//!     .run_warm_fork(Some("my-pool-key"), None)
//!     .await?;
//! println!("sandbox {} started", result.sandbox_id);
//! # Ok(())
//! # }
//! ```

pub mod sandbox;
pub mod template;

mod client;
