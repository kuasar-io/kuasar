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
use vmm_client::{sandbox::SandboxApi, template::TemplateApi};

mod sandbox;

const DEFAULT_ADMIN_SOCK: &str = "/run/vmm-sandboxer-admin.sock";

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
    /// Manage VM template snapshots in the template pool
    Template {
        #[command(subcommand)]
        action: TemplateAction,
    },
    /// Manage sandboxes via the admin socket
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
    /// Create a container snapshot template from a running sandbox.
    ///
    /// With --sandbox <id>: the running sandbox's VM is paused, snapshotted, and
    /// immediately resumed so the original container keeps running.
    ///
    /// Note: bare-VM snapshots are managed automatically by the sandboxer and
    /// cannot be created via this command.
    Create {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Sandbox ID to snapshot
        #[arg(long)]
        sandbox_id: String,
        /// Snapshot type: "warm_fork" (default) or "continuation"
        #[arg(long, default_value = "warm_fork")]
        snapshot_type: String,
        /// Template pool key — required for warm_fork; ignored for continuation
        /// (key is auto-derived from pod_uid). Pods with
        /// `kuasar.io/template-key=<key>` will be matched to this template.
        #[arg(long)]
        key: Option<String>,
        /// Pod UID — required for continuation (kubectl get pod -o jsonpath='{.metadata.uid}')
        #[arg(long)]
        pod_uid: Option<String>,
        /// Workload generation — continuation only; defaults to 0
        #[arg(long, default_value_t = 0)]
        generation: u32,
    },
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
    Refill {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Template kind to refill: only "environment" is supported (required)
        #[arg(long)]
        kind: String,
        /// Target pool depth after refill
        #[arg(long)]
        target_depth: usize,
    },
    /// Remove a single template from the pool by ID.
    ///
    /// Both --kind and --template-id are required. --kind is a safety cross-check
    /// against the template's actual type and prevents accidental deletion.
    Gc {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Template kind: "environment" or "warm_fork" (required)
        #[arg(long)]
        kind: String,
        /// Template ID to remove (required)
        #[arg(long)]
        template_id: String,
    },
}

#[derive(clap::Subcommand)]
enum SandboxAction {
    /// List all sandboxes known to the sandboxer.
    List {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
    },
    /// Get details of a specific sandbox.
    Get {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Sandbox ID to query
        #[arg(long)]
        id: String,
    },
    /// Run a new sandbox from a template snapshot (create slot + restore in one step).
    ///
    /// The sandboxer auto-creates the sandbox slot and restores the VM state from the template.
    /// The assigned sandbox ID is printed to stdout on success.
    ///
    /// Valid combinations per snapshot type:
    ///
    ///   warm_fork    --template-key <key>            (latest template matching this key)
    ///                --template-id <id>              (specific template by ID)
    ///                --template-key <key> --template-id <id>  (both must match)
    ///   continuation --pod-uid <uid> [--generation <n>]
    Run {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Snapshot type: "warm_fork" or "continuation" (required)
        #[arg(long)]
        snapshot_type: String,
        /// warm_fork: pin to a specific template by ID (can be combined with --template-key).
        #[arg(long)]
        template_id: Option<String>,
        /// warm_fork: pool key — restore the latest template matching this key
        #[arg(long)]
        template_key: Option<String>,
        /// Continuation: pod UID (kubectl get pod -o jsonpath='{.metadata.uid}')
        #[arg(long)]
        pod_uid: Option<String>,
        /// Continuation: workload generation (default 0)
        #[arg(long)]
        generation: Option<u32>,
    },
    /// Destroy a running sandbox: stop the VM, release the template lease, and delete all files.
    ///
    /// Works for sandboxes created by `sandbox run` as well as containerd-managed sandboxes
    /// that need to be cleaned up via the admin socket.
    Destroy {
        /// Admin socket of the running vmm-sandboxer
        #[arg(long, default_value = DEFAULT_ADMIN_SOCK)]
        admin_sock: PathBuf,
        /// Sandbox ID to destroy
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
        Commands::Template {
            action:
                TemplateAction::Create {
                    admin_sock,
                    sandbox_id,
                    snapshot_type,
                    key,
                    pod_uid,
                    generation,
                },
        } => {
            match snapshot_type.as_str() {
                "warm_fork" => {
                    let k = key.ok_or_else(|| {
                        anyhow!("--key is required for --snapshot-type warm_fork")
                    })?;
                    let info = TemplateApi::new(&admin_sock)
                        .create_from_sandbox(&sandbox_id, &k)
                        .await?;
                    println!(
                        "template {} created from sandbox {} (key={})",
                        info.template_id, sandbox_id, info.key
                    );
                }
                "continuation" => {
                    let uid = pod_uid.ok_or_else(|| {
                        anyhow!("--pod-uid is required for --snapshot-type continuation")
                    })?;
                    let info = TemplateApi::new(&admin_sock)
                        .create_continuation_from_sandbox(&sandbox_id, &uid, generation)
                        .await?;
                    println!(
                        "continuation template {} created from sandbox {} (pod_uid={}, generation={}, key={})",
                        info.template_id, sandbox_id, uid, generation, info.key
                    );
                }
                other => {
                    return Err(anyhow!(
                        "unknown --snapshot-type '{}'; use warm_fork or continuation",
                        other
                    ));
                }
            }
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::List { admin_sock },
        } => {
            let sandboxes = SandboxApi::new(&admin_sock).list().await?;
            if sandboxes.is_empty() {
                println!("no sandboxes");
            } else {
                println!(
                    "{:<64}  {:<10}  {:<13}  {:<32}",
                    "SANDBOX ID", "STATUS", "SNAPSHOT TYPE", "TEMPLATE ID"
                );
                for sb in &sandboxes {
                    println!(
                        "{:<64}  {:<10}  {:<13}  {}",
                        sb.id,
                        sb.status,
                        sb.template_snapshot_type.as_deref().unwrap_or("-"),
                        sb.template_id.as_deref().unwrap_or("-"),
                    );
                }
            }
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::Get { admin_sock, id },
        } => {
            let sb = SandboxApi::new(&admin_sock).get(&id).await?;
            println!("{}", serde_json::to_string_pretty(&sb)?);
            Ok(0)
        }
        Commands::Sandbox {
            action:
                SandboxAction::Run {
                    admin_sock,
                    snapshot_type,
                    template_id,
                    template_key,
                    pod_uid,
                    generation,
                },
        } => {
            let result = match snapshot_type.as_str() {
                "warm_fork" => {
                    if pod_uid.is_some() || generation.is_some() {
                        return Err(anyhow!(
                            "snapshot_type=warm_fork does not accept --pod-uid or --generation"
                        ));
                    }
                    if template_id.is_none() && template_key.is_none() {
                        return Err(anyhow!(
                            "snapshot_type=warm_fork requires at least one of \
                             --template-id or --template-key"
                        ));
                    }
                    SandboxApi::new(&admin_sock)
                        .run_warm_fork(template_key.as_deref(), template_id.as_deref())
                        .await?
                }
                "continuation" => {
                    if template_id.is_some() || template_key.is_some() {
                        return Err(anyhow!(
                            "snapshot_type=continuation only accepts --pod-uid and --generation; \
                             --template-id and --template-key are not valid for this type"
                        ));
                    }
                    let uid = pod_uid.ok_or_else(|| {
                        anyhow!("--pod-uid is required for --snapshot-type continuation")
                    })?;
                    let gen = generation.unwrap_or(0);
                    SandboxApi::new(&admin_sock)
                        .run_continuation(&uid, gen)
                        .await?
                }
                other => {
                    return Err(anyhow!(
                        "unknown --snapshot-type '{}'; valid values: warm_fork, continuation",
                        other
                    ));
                }
            };
            println!("{}", result.sandbox_id);
            Ok(0)
        }
        Commands::Sandbox {
            action: SandboxAction::Destroy { admin_sock, id },
        } => {
            SandboxApi::new(&admin_sock).destroy(&id).await?;
            println!("sandbox {} destroyed", id);
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
                    kind,
                    target_depth,
                },
        } => {
            let r = TemplateApi::new(&admin_sock)
                .refill(&kind, target_depth)
                .await?;
            println!(
                "pool refill queued: kind={}, target_depth={}, in_flight={}",
                r.kind, target_depth, r.in_flight
            );
            Ok(0)
        }
        Commands::Pool {
            action:
                PoolAction::Gc {
                    admin_sock,
                    kind,
                    template_id,
                },
        } => {
            let r = TemplateApi::new(&admin_sock)
                .gc(&kind, &template_id)
                .await?;
            println!(
                "pool gc: kind={}, template_id={}, removed={}, remaining={}",
                r.kind, template_id, r.removed, r.remaining
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
