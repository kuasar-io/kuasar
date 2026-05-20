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

//! WarmFork injection protocol message types and framing.
//!
//! Both the guest agent (vmm-task) and any future workload SDK use these types.
//! The protocol spec lives in `vmm-sandboxer::warmfork::protocol`.
//!
//! ## Two-phase protocol summary
//!
//! ```text
//! Workload                        Sandboxer (via guest agent)
//! ────────                        ───────────────────────────
//! → CAPABILITIES                  read CAPABILITIES; verify version
//!                        ←        PREPARE(task_id, env, context)
//! validate (no side effects)
//! → READY                         [barrier: all targets must be READY]
//!                        ←        COMMIT
//! → STARTED                       restore committed; lease locked
//! close; begin execution
//! ```
//!
//! On rollback the sandboxer sends CANCEL in place of PREPARE, or after READY
//! if another target sent REJECT. CANCEL is never sent after COMMIT.

use std::{collections::HashMap, io};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol version string declared in `CAPABILITIES` and stored in `PooledTemplate`.
pub const PROTOCOL_VERSION: &str = "1";

/// Maximum allowed frame body size (4 MiB). Prevents OOM on malformed frames.
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

// ── Messages sent by the workload to the sandboxer ───────────────────────────

/// Messages the workload sends to the sandboxer (guest agent).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WorkloadMessage {
    /// First message sent after `accept()` returns. Declares protocol version
    /// and supported features. Sent on every connection, including probe connections.
    #[serde(rename = "CAPABILITIES")]
    Capabilities(Capabilities),

    /// Workload has validated the PREPARE payload and commits to producing no
    /// externally-visible side effects before this message is sent.
    /// Workload blocks until it receives COMMIT or CANCEL.
    #[serde(rename = "READY")]
    Ready,

    /// Workload has received COMMIT and is acknowledging it.
    /// **Formal restore commit point**: sandboxer locks the lease on receipt.
    /// Sent BEFORE execution begins; workload closes the connection immediately after.
    #[serde(rename = "STARTED")]
    Started,

    /// Workload cannot process the PREPARE payload. Sandboxer sends CANCEL to
    /// already-READY targets and rolls back the VM.
    #[serde(rename = "REJECT")]
    Reject {
        reason: RejectReason,
        #[serde(default)]
        message: String,
    },

    /// Optional acknowledgement of CANCEL. Workload may send this after receiving
    /// CANCEL to confirm it has acknowledged the rollback. Omitting it is fine.
    #[serde(rename = "CANCEL_ACK")]
    CancelAck,
}

/// Workload capabilities. Sent as the first message on every connection.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Capabilities {
    /// Must equal [`PROTOCOL_VERSION`].
    pub protocol_version: String,
    /// Feature strings this workload supports. Unknown strings are silently ignored.
    #[serde(default)]
    pub supported_features: Vec<String>,
}

/// Reason codes for `REJECT` messages.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectReason {
    /// The `PREPARE` payload could not be parsed as valid JSON.
    ParseError,
    /// The `task_id` field is absent or empty.
    InvalidTaskId,
    /// The workload cannot accept more tasks at this time.
    ResourceExhausted,
}

impl RejectReason {
    /// Returns the stable snake_case wire string for this reason.
    pub fn as_str(self) -> &'static str {
        match self {
            RejectReason::ParseError => "parse_error",
            RejectReason::InvalidTaskId => "invalid_task_id",
            RejectReason::ResourceExhausted => "resource_exhausted",
        }
    }
}

// ── Messages sent by the sandboxer (guest agent) to the workload ─────────────

/// Messages the sandboxer (via guest agent) sends to the workload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SandboxerMessage {
    /// Phase-1: delivers per-task parameters for validation.
    /// Workload must validate but must NOT produce any externally-visible side effect
    /// before replying READY. Unknown fields must be ignored.
    #[serde(rename = "PREPARE")]
    Prepare(PreparePayload),

    /// Phase-2: all targets have replied READY; instructs all targets to begin execution.
    /// On receiving COMMIT the workload sends STARTED and then starts executing.
    /// COMMIT is never rolled back.
    #[serde(rename = "COMMIT")]
    Commit,

    /// Rollback signal. Sent in place of PREPARE, or after PREPARE if another target
    /// sent REJECT. Never sent after COMMIT.
    /// Best-effort: workload may not receive it if it has already left the wait point.
    #[serde(rename = "CANCEL")]
    Cancel {
        #[serde(default)]
        reason: String,
    },
}

/// Per-task parameters injected into the workload during Phase 1 (PREPARE).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct PreparePayload {
    /// Unique identifier for this task instance. Non-empty.
    pub task_id: String,
    /// Environment variable overrides. Applied by the workload after COMMIT.
    #[serde(default)]
    pub env_overrides: HashMap<String, String>,
    /// Opaque context for workload use (e.g. serialized prompt, routing key).
    #[serde(default)]
    pub context: String,
}

// ── Framing ───────────────────────────────────────────────────────────────────

/// Reads one length-prefixed message from `reader`.
///
/// Frame format: `[4-byte big-endian length][JSON body]`
///
/// Returns `Err` if the frame length exceeds `max_len`, the read fails, or
/// the JSON cannot be deserialized into `T`.
pub async fn read_framed_message<R, T>(reader: &mut R, max_len: u32) -> io::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds maximum {}", len, max_len),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes one length-prefixed message to `writer`.
///
/// Frame format: `[4-byte big-endian length][JSON body]`
pub async fn write_framed_message<W, T>(writer: &mut W, msg: &T) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_payload_roundtrip() {
        let mut env = HashMap::new();
        env.insert("KEY".to_string(), "VAL".to_string());
        let payload = PreparePayload {
            task_id: "req-1".to_string(),
            env_overrides: env,
            context: "ctx".to_string(),
        };
        let msg = SandboxerMessage::Prepare(payload.clone());
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"PREPARE\""));
        assert!(json.contains("req-1"));
    }

    #[test]
    fn capabilities_roundtrip() {
        let caps = WorkloadMessage::Capabilities(Capabilities {
            protocol_version: PROTOCOL_VERSION.to_string(),
            supported_features: vec!["prepare".to_string(), "commit".to_string()],
        });
        let json = serde_json::to_string(&caps).unwrap();
        let back: WorkloadMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, WorkloadMessage::Capabilities(_)));
    }

    #[test]
    fn ready_roundtrip() {
        let msg = WorkloadMessage::Ready;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"READY\""));
        let back: WorkloadMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, WorkloadMessage::Ready));
    }

    #[test]
    fn started_roundtrip() {
        let msg = WorkloadMessage::Started;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"STARTED\""));
        let back: WorkloadMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, WorkloadMessage::Started));
    }

    #[test]
    fn commit_roundtrip() {
        let msg = SandboxerMessage::Commit;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"COMMIT\""));
        let back: SandboxerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, SandboxerMessage::Commit));
    }

    #[test]
    fn reject_roundtrip() {
        let msg = WorkloadMessage::Reject {
            reason: RejectReason::InvalidTaskId,
            message: "task_id is empty".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"REJECT\""));
        assert!(json.contains("invalid_task_id"));
    }

    #[test]
    fn unknown_fields_ignored() {
        let json = r#"{"type":"READY","future_field":"ignored"}"#;
        let msg: WorkloadMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, WorkloadMessage::Ready));
    }

    #[test]
    fn cancel_roundtrip() {
        let msg = SandboxerMessage::Cancel {
            reason: "rollback".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"CANCEL\""));
    }
}
