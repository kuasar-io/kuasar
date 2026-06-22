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

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout as tokio_timeout;
use vmm_client::{sandbox::SandboxApi, snapshot::SnapshotApi, template::TemplateApi};

mod sandbox;

const DEFAULT_ADMIN_SOCK: &str = "/run/vmm-sandboxer-admin.sock";
const DEFAULT_GRPC_SOCK: &str = "/run/vmm-sandboxer-service.sock";

const EXIT_MARKER: &str = "__KSR_EXIT__";
const INTERRUPTED_EXIT_CODE: i32 = 130;
const TIMED_OUT_EXIT_CODE: i32 = 124;

#[derive(Parser)]
#[command(author, version, about = "Kuasar diagnostic tool aligned with kata-ctl", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Manage sandbox snapshots via the gRPC snapshot service
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Execute a command in a Cloud Hypervisor guest via debug console (hvsock)
    Exec {
        /// Sandbox ID
        sandbox: String,
        /// Command to execute
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Debug console vport (default 1025)
        #[arg(short = 'p', long = "vport", default_value_t = 1025)]
        vport: u32,
        /// Timeout in seconds (optional)
        #[arg(short = 't', long = "timeout")]
        timeout: Option<u64>,
    },
    /// Inspect templates in the template pool
    Template {
        #[command(subcommand)]
        action: TemplateAction,
    },
    /// Manage sandbox instances via the gRPC snapshot service
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },
    /// Manage template pool operations
    Pool {
        #[command(subcommand)]
        action: PoolAction,
    },
}

#[derive(clap::Subcommand)]
enum TemplateAction {
    /// List available templates.
    List {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
    },
    /// Get details of a specific template.
    Get {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Template ID to query
        #[arg(long)]
        id: String,
    },
}

#[derive(clap::Subcommand)]
enum PoolAction {
    /// Query template pool status and metrics.
    Status {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
    },
    /// Force a pool refill for environment templates up to target_depth.
    ///
    /// WarmFork and Continuation snapshots are created via 'snapshot create'.
    Refill {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Target pool depth after refill
        #[arg(long)]
        target_depth: usize,
    },
    /// Remove a single environment template from the pool by ID.
    ///
    /// WarmFork and Continuation snapshots are removed via 'snapshot delete'.
    Gc {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Template ID to remove
        #[arg(long)]
        template_id: String,
    },
}

#[derive(clap::Subcommand)]
enum SnapshotAction {
    /// Create a snapshot from a running pod.
    Create {
        /// gRPC socket of the snapshot service
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Logical snapshot name (idempotency key).
        /// Optional in continuation mode: omit to auto-derive from pod_uid + generation.
        /// Required in warm_fork mode.
        #[arg(long)]
        name: Option<String>,
        /// Kubernetes Pod UID of the running pod to snapshot
        #[arg(long)]
        pod_uid: String,
        /// Snapshot mode: "warm_fork" (default) or "continuation"
        #[arg(long, default_value = "warm_fork")]
        mode: String,
        /// Workload generation — continuation only (default 0)
        #[arg(long)]
        generation: Option<u64>,
    },
    /// Delete a snapshot by its snapshot ID.
    Delete {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Snapshot name to delete
        #[arg(long)]
        snapshot_name: String,
    },
    /// List all snapshots, optionally filtered by mode.
    List {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Filter by mode: "warm_fork" or "continuation" (omit for all)
        #[arg(long)]
        mode: Option<String>,
    },
    /// Get details of a specific snapshot by name.
    Get {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Snapshot name to query
        #[arg(long)]
        name: String,
    },
    /// Check the health of the snapshot service.
    Probe {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
    },
    /// Show plugin name and version.
    Info {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
    },
}

#[derive(clap::Subcommand)]
enum SandboxAction {
    /// Checkpoint a running sandbox to disk and stop the CH process.
    /// Network state (tap, TC rules, netns) is preserved; use 'resume' to restore.
    Pause {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Sandbox ID to pause
        #[arg(long)]
        id: String,
    },
    /// Resume a previously paused sandbox VM.
    Resume {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Sandbox ID to resume
        #[arg(long)]
        id: String,
    },
    /// List all sandbox instances on this node.
    List {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
    },
    /// Get details of a specific sandbox.
    Get {
        #[arg(long, default_value = DEFAULT_GRPC_SOCK)]
        grpc_sock: PathBuf,
        /// Sandbox ID to query
        #[arg(long)]
        id: String,
    },
}

#[tokio::main]
async fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    let exit_code = match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err:#}");
            1
        }
    };

    std::process::exit(exit_code);
}

async fn run() -> Result<i32> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Snapshot {
            action:
                SnapshotAction::Create {
                    grpc_sock,
                    name,
                    pod_uid,
                    mode,
                    generation,
                },
        } => {
            if mode == "warm_fork" && name.is_none() {
                eprintln!("error: --name is required for warm_fork mode");
                return Ok(1);
            }
            let info = SnapshotApi::new(&grpc_sock)
                .create(name.as_deref().unwrap_or(""), &pod_uid, &mode, generation)
                .await?;
            println!(
                "snapshot created: name={} pod_uid={} mode={}",
                info.snapshot_name, info.pod_uid, info.mode
            );
            Ok(0)
        }
        Commands::Snapshot {
            action:
                SnapshotAction::Delete {
                    grpc_sock,
                    snapshot_name,
                },
        } => {
            SnapshotApi::new(&grpc_sock).delete(&snapshot_name).await?;
            println!("snapshot {} deleted", snapshot_name);
            Ok(0)
        }
        Commands::Snapshot {
            action: SnapshotAction::List { grpc_sock, mode },
        } => {
            let snapshots = SnapshotApi::new(&grpc_sock).list(mode.as_deref()).await?;
            if snapshots.is_empty() {
                println!("No resources found.");
            } else {
                println!("{:<44}  {:<36}  MODE", "NAME", "POD UID");
                for s in &snapshots {
                    println!("{:<44}  {:<36}  {}", s.snapshot_name, s.pod_uid, s.mode,);
                }
            }
            Ok(0)
        }
        Commands::Snapshot {
            action: SnapshotAction::Get { grpc_sock, name },
        } => match SnapshotApi::new(&grpc_sock).get(&name).await? {
            Some(s) => {
                println!(
                    "snapshot_name: {}\npod_uid: {}\nmode: {}",
                    s.snapshot_name, s.pod_uid, s.mode
                );
                Ok(0)
            }
            None => {
                eprintln!("snapshot '{}' not found", name);
                Ok(1)
            }
        },
        Commands::Snapshot {
            action: SnapshotAction::Probe { grpc_sock },
        } => {
            let ready = SnapshotApi::new(&grpc_sock).probe().await?;
            println!("ready: {}", ready);
            Ok(if ready { 0 } else { 1 })
        }
        Commands::Snapshot {
            action: SnapshotAction::Info { grpc_sock },
        } => {
            let info = SnapshotApi::new(&grpc_sock).info().await?;
            println!("name: {}\nversion: {}", info.name, info.version);
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::Pause { grpc_sock, id },
        } => {
            SandboxApi::new(&grpc_sock).pause(&id).await?;
            println!("sandbox {} paused", id);
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::Resume { grpc_sock, id },
        } => {
            SandboxApi::new(&grpc_sock).resume(&id).await?;
            println!("sandbox {} resumed", id);
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::List { grpc_sock },
        } => {
            let sandboxes = SandboxApi::new(&grpc_sock).list().await?;
            if sandboxes.is_empty() {
                println!("No resources found.");
            } else {
                println!(
                    "{:<36}  {:<64}  {:<10}  {:<13}  SNAPSHOT NAME",
                    "POD UID", "SANDBOX ID", "STATUS", "SNAPSHOT MODE"
                );
                for sb in &sandboxes {
                    println!(
                        "{:<36}  {:<64}  {:<10}  {:<13}  {}",
                        sb.pod_uid,
                        sb.sandbox_id,
                        sb.status,
                        sb.snapshot_mode,
                        if sb.snapshot_name.is_empty() {
                            "-"
                        } else {
                            &sb.snapshot_name
                        },
                    );
                }
            }
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::Get { grpc_sock, id },
        } => {
            let sb = SandboxApi::new(&grpc_sock).get(&id).await?;
            println!(
                "pod_uid: {}\nsandbox_id: {}\nstatus: {}\nsnapshot_mode: {}\nsnapshot_name: {}",
                sb.pod_uid,
                sb.sandbox_id,
                sb.status,
                sb.snapshot_mode,
                if sb.snapshot_name.is_empty() {
                    "-"
                } else {
                    &sb.snapshot_name
                },
            );
            Ok(0)
        }
        Commands::Template {
            action: TemplateAction::List { admin_sock },
        } => {
            let templates = TemplateApi::new(&admin_sock).list().await?;
            if templates.is_empty() {
                println!("no templates");
            } else {
                let key_w = templates
                    .iter()
                    .map(|t| t.0["key"].as_str().unwrap_or("-").len())
                    .max()
                    .unwrap_or(0)
                    .max("KEY".len());
                println!(
                    "{:<36}  {:<13}  {:<key_w$}  SNAPSHOT DIR",
                    "TEMPLATE ID", "SNAPSHOT TYPE", "KEY"
                );
                for t in &templates {
                    let v = &t.0;
                    println!(
                        "{:<36}  {:<13}  {:<key_w$}  {}",
                        v["template_id"].as_str().unwrap_or("-"),
                        v["snapshot_type"].as_str().unwrap_or("-"),
                        v["key"].as_str().unwrap_or("-"),
                        v["snapshot_dir"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(0)
        }
        Commands::Template {
            action: TemplateAction::Get { admin_sock, id },
        } => {
            let template = TemplateApi::new(&admin_sock).get(&id).await?;
            println!("{}", serde_json::to_string_pretty(&template.0)?);
            Ok(0)
        }
        Commands::Pool {
            action: PoolAction::Status { admin_sock },
        } => {
            let status = TemplateApi::new(&admin_sock).pool_status().await?;
            println!("{}", serde_json::to_string_pretty(&status.0)?);
            Ok(0)
        }
        Commands::Pool {
            action:
                PoolAction::Refill {
                    admin_sock,
                    target_depth,
                },
        } => {
            let r = TemplateApi::new(&admin_sock).refill(target_depth).await?;
            println!(
                "pool refill queued: target_depth={}, in_flight={}",
                target_depth, r.in_flight
            );
            Ok(0)
        }
        Commands::Pool {
            action:
                PoolAction::Gc {
                    admin_sock,
                    template_id,
                },
        } => {
            let r = TemplateApi::new(&admin_sock).gc(&template_id).await?;
            println!(
                "pool gc: template_id={}, removed={}, remaining={}",
                template_id, r.removed, r.remaining
            );
            Ok(0)
        }
        Commands::Exec {
            sandbox,
            command,
            vport,
            timeout,
        } => {
            let target = sandbox::resolve_sandbox(&sandbox)?;
            let hvsock_path = match target {
                sandbox::SandboxTarget::CloudHypervisor { dir } => sandbox::get_hvsock_path(&dir),
            };

            if !hvsock_path.exists() {
                return Err(anyhow!(
                    "hvsock socket {:?} not found for Cloud Hypervisor sandbox. Is the sandbox running with debug=true?",
                    hvsock_path
                ));
            }

            let fut = handle_exec(&hvsock_path, command, vport);

            tokio::select! {
                res = async {
                    if let Some(t) = timeout {
                        match tokio_timeout(Duration::from_secs(t), fut).await {
                            Ok(res) => res,
                            Err(_) => {
                                eprintln!("command execution timed out after {} seconds", t);
                                Ok(TIMED_OUT_EXIT_CODE)
                            }
                        }
                    } else {
                        fut.await
                    }
                } => res,
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("\nInterrupted by user, terminating connection...");
                    Ok(INTERRUPTED_EXIT_CODE)
                }
            }
        }
    }
}

async fn handle_exec(
    hvsock_path: &std::path::Path,
    command: Vec<String>,
    vport: u32,
) -> Result<i32> {
    let mut stream = UnixStream::connect(hvsock_path)
        .await
        .context(format!("failed to connect to hvsock: {:?}", hvsock_path))?;

    // HVSOCK Handshake
    let handshake = format!("CONNECT {}\n", vport);
    stream.write_all(handshake.as_bytes()).await?;

    // Read handshake response byte-by-byte to avoid buffering guest output
    let mut response = String::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            return Err(anyhow!("hvsock connection closed during handshake"));
        }
        response.push(byte[0] as char);
        if byte[0] == b'\n' {
            break;
        }
    }

    if !response.trim().starts_with("OK") {
        return Err(anyhow!(
            "hvsock handshake failed: expected OK, got {:?}",
            response
        ));
    }

    // Send protocol mode header to guest debug console
    stream.write_all(b"KSR_MODE exec\n").await?;

    execute_non_interactive(&mut stream, &command).await
}

async fn execute_non_interactive(
    stream: &mut tokio::net::UnixStream,
    command: &[String],
) -> Result<i32> {
    let token = generate_exec_token();
    let payload = build_exec_payload(command, &token)?;
    stream.write_all(payload.as_bytes()).await?;

    let mut parser = ExitCodeParser::new(&token);
    let mut stdout = tokio::io::stdout();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break, // Connection closed (exit)
            Ok(n) => {
                let output = parser.push(&buffer[..n]);
                write_stdout(&mut stdout, &output).await?;
            }
            Err(e) => {
                return Err(anyhow!("read error from VM debug console: {}", e));
            }
        }
    }

    let finished = parser.finish();
    write_stdout(&mut stdout, &finished.output).await?;

    finished
        .exit_code
        .ok_or_else(|| anyhow!("failed to detect guest command exit code"))
}

fn build_exec_payload(command: &[String], token: &str) -> Result<String> {
    if command.is_empty() {
        return Err(anyhow!("command is required"));
    }

    let wrapper = format!(
        "\"$@\"; rc=$?; printf '%s:%s:%s\\n' '{}' '{}' \"$rc\"; exit \"$rc\"",
        EXIT_MARKER, token
    );
    let escaped = command
        .iter()
        .map(|arg| shell_escape_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "exec sh -c {} 'sh' {}\n",
        shell_escape_arg(&wrapper),
        escaped
    ))
}

fn shell_escape_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }

    let mut escaped = String::from("'");
    for ch in arg.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

fn generate_exec_token() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{}", std::process::id(), ts)
}

async fn write_stdout(stdout: &mut tokio::io::Stdout, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }

    stdout.write_all(bytes).await?;
    stdout.flush().await?;
    Ok(())
}

struct ExitCodeParser {
    tail: Vec<u8>,
    marker_prefix: Vec<u8>,
    max_marker_len: usize,
}

struct FinishResult {
    output: Vec<u8>,
    exit_code: Option<i32>,
}

impl ExitCodeParser {
    fn new(token: &str) -> Self {
        let marker_prefix = format!("{EXIT_MARKER}:{token}:").into_bytes();
        let max_marker_len = marker_prefix.len() + 5;
        Self {
            tail: Vec::new(),
            marker_prefix,
            max_marker_len,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() <= self.max_marker_len {
            return Vec::new();
        }

        let split_at = self.tail.len() - self.max_marker_len;
        self.tail.drain(..split_at).collect()
    }

    fn finish(self) -> FinishResult {
        if let Some(start) = rfind_subslice(&self.tail, &self.marker_prefix) {
            let suffix = &self.tail[start + self.marker_prefix.len()..];
            let digits = if suffix.ends_with(b"\r\n") {
                &suffix[..suffix.len() - 2]
            } else if suffix.ends_with(b"\n") {
                &suffix[..suffix.len() - 1]
            } else {
                suffix
            };
            if !digits.is_empty() && digits.iter().all(|b| b.is_ascii_digit()) {
                let exit_code = std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok());
                return FinishResult {
                    output: self.tail[..start].to_vec(),
                    exit_code,
                };
            }
        }

        FinishResult {
            output: self.tail,
            exit_code: None,
        }
    }
}

fn rfind_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }

    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::{build_exec_payload, shell_escape_arg, ExitCodeParser, EXIT_MARKER};

    #[test]
    fn shell_escape_handles_single_quotes() {
        assert_eq!(shell_escape_arg("a'b"), "'a'\\''b'");
    }

    #[test]
    fn build_exec_payload_preserves_argument_boundaries() {
        let payload = build_exec_payload(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf '%s %s\\n' \"$1\" \"$2\"".to_string(),
                "hello world".to_string(),
                "x'y".to_string(),
            ],
            "token-123",
        )
        .unwrap();

        assert!(payload.starts_with("exec sh -c '"));
        assert!(payload.contains(EXIT_MARKER));
        assert!(payload.contains("token-123"));
        assert!(payload.contains("printf"));
        assert!(payload.ends_with(
            "'sh' 'sh' '-c' 'printf '\\''%s %s\\n'\\'' \"$1\" \"$2\"' 'hello world' 'x'\\''y'\n"
        ));
    }

    #[test]
    fn parser_extracts_exit_code_across_chunks() {
        let token = "token-123";
        let mut parser = ExitCodeParser::new(token);
        let plain_output = "0123456789abcdefghijklmnopqrstuvwxyz";

        let out1 = parser.push(plain_output.as_bytes());
        let out2 = parser.push(format!("{EXIT_MARKER}:{token}:4").as_bytes());
        let out3 = parser.push(b"2\r\n");
        let finish = parser.finish();

        let mut combined = Vec::new();
        combined.extend_from_slice(&out1);
        combined.extend_from_slice(&out2);
        combined.extend_from_slice(&out3);
        combined.extend_from_slice(&finish.output);

        assert_eq!(String::from_utf8(combined).unwrap(), plain_output);
        assert_eq!(finish.exit_code, Some(42));
    }

    #[test]
    fn parser_preserves_output_when_marker_missing() {
        let mut parser = ExitCodeParser::new("token-123");

        let out = parser.push(b"hello");
        let finish = parser.finish();

        assert!(out.is_empty());
        assert_eq!(String::from_utf8(finish.output).unwrap(), "hello");
        assert_eq!(finish.exit_code, None);
    }
}
