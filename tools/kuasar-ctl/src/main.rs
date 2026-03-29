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
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout as tokio_timeout;

mod sandbox;

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
        /// Sandbox ID or prefix
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
