/*
Copyright 2025 The Kuasar Authors.

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

//! Kuasar End-to-End Testing Framework
//!
//! This library provides utilities for running comprehensive end-to-end tests
//! for the Kuasar container runtime, following Rust testing patterns.
//!
//! # Example Usage
//!
//! ```rust,no_run
//! use kuasar_e2e::E2EContext;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut ctx = E2EContext::new().await?;
//!     
//!     // Start services
//!     ctx.start_services(&["runc"]).await?;
//!     
//!     // Run tests
//!     let result = ctx.test_runtime("runc").await?;
//!     println!("Test result: {:?}", result);
//!     
//!     // Always cleanup explicitly
//!     ctx.cleanup().await?;
//!     
//!     Ok(())
//! }
//! ```

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Configuration for e2e test execution
#[derive(Debug, Clone)]
pub struct E2EConfig {
    /// Root directory of the Kuasar repository
    pub repo_root: PathBuf,
    /// Directory to store test artifacts
    pub artifacts_dir: PathBuf,
    /// Directory containing test configuration files
    pub config_dir: PathBuf,
    /// Socket paths for different runtimes
    pub sockets: HashMap<String, String>,
    /// Test timeout duration
    pub timeout: Duration,
    /// Log level for tests
    pub log_level: String,
}

impl Default for E2EConfig {
    fn default() -> Self {
        let repo_root = find_repo_root().expect("Failed to find repository root");
        let artifacts_dir = std::env::var("ARTIFACTS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("kuasar-e2e-artifacts"));

        let config_dir = repo_root.join("tests").join("e2e").join("configs");

        let mut socket_map = HashMap::new();
        socket_map.insert("runc".to_string(), "/run/kuasar-runc.sock".to_string());

        Self {
            repo_root,
            artifacts_dir,
            config_dir,
            sockets: socket_map,
            timeout: Duration::from_secs(300), // 5 minutes default timeout
            log_level: "info".to_string(),
        }
    }
}

/// Test runtime configuration
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub name: String,
    pub sandbox_config: PathBuf,
    pub container_config: PathBuf,
    pub socket_path: String,
}

/// Kuasar E2E test context
pub struct E2EContext {
    pub config: E2EConfig,
    pub test_id: String,
    services_started: bool,
    child_process: Option<tokio::process::Child>,
}

impl E2EContext {
    /// Create a new E2E test context
    pub async fn new() -> Result<Self> {
        Self::new_with_config(E2EConfig::default()).await
    }

    /// Create a new E2E test context with custom configuration
    pub async fn new_with_config(config: E2EConfig) -> Result<Self> {
        // Ensure artifacts directory exists
        fs::create_dir_all(&config.artifacts_dir)
            .await
            .context("Failed to create artifacts directory")?;

        let test_id = format!("kuasar-e2e-{}", Uuid::new_v4().simple());

        Ok(Self {
            config,
            test_id,
            services_started: false,
            child_process: None,
        })
    }

    /// Start Kuasar services for testing
    pub async fn start_services(&mut self, components: &[&str]) -> Result<()> {
        if self.services_started {
            return Ok(());
        }

        info!("Starting Kuasar services: {:?}", components);

        let local_up_script = self
            .config
            .repo_root
            .join("hack")
            .join("local-up-kuasar.sh");
        let components_arg = components.join(",");

        let mut cmd = TokioCommand::new(&local_up_script);
        cmd.args(&["--components", &components_arg])
            .current_dir(&self.config.repo_root)
            .env("ARTIFACTS", &self.config.artifacts_dir)
            .env("KUASAR_LOG_LEVEL", &self.config.log_level)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        info!("Start command: {:?}", cmd);
        self.child_process = Some(cmd.spawn().context("Failed to start local-up-kuasar.sh")?);
        let child_stdout = self
            .child_process
            .as_mut()
            .unwrap()
            .stdout
            .take()
            .expect("Failed to capture stdout");
        let child_stderr = self
            .child_process
            .as_mut()
            .unwrap()
            .stderr
            .take()
            .expect("Failed to capture stderr");

        // Read stdout and stderr in separate tasks
        tokio::spawn(async move {
            let mut stdout = String::new();
            let mut reader = tokio::io::BufReader::new(child_stdout);
            while let Ok(n) = reader.read_line(&mut stdout).await {
                if n == 0 {
                    break;
                }
                info!("stdout: {}", stdout.trim());
                stdout.clear();
            }
        });
        tokio::spawn(async move {
            let mut stderr = String::new();
            let mut reader = tokio::io::BufReader::new(child_stderr);
            while let Ok(n) = reader.read_line(&mut stderr).await {
                if n == 0 {
                    break;
                }
                error!("stderr: {}", stderr.trim());
                stderr.clear();
            }
        });

        // Wait for stdout and stderr tasks to complete
        //let (stdout_result, stderr_result) = tokio::try_join!(stdout_task, stderr_task)?;

        // Wait a bit for services to start
        sleep(Duration::from_secs(5)).await;

        info!("Waiting for Kuasar services to be ready...");
        // Wait for services to be ready
        self.wait_for_services_ready(components).await?;

        self.services_started = true;
        info!("Kuasar services started successfully");

        Ok(())
    }

    /// Wait for services to be ready by checking socket files
    async fn wait_for_services_ready(&self, components: &[&str]) -> Result<()> {
        let start_time = Instant::now();

        while start_time.elapsed() < self.config.timeout {
            let mut all_ready = true;

            for component in components {
                if let Some(socket_path) = self.config.sockets.get(*component) {
                    if !Path::new(socket_path).exists() {
                        debug!("Waiting for {} socket: {}", component, socket_path);
                        all_ready = false;
                        break;
                    }
                }
            }

            if all_ready {
                info!("All services are ready");
                return Ok(());
            }

            sleep(Duration::from_secs(2)).await;
        }

        anyhow::bail!("Services failed to start within timeout period")
    }

    /// Get runtime configuration for a specific runtime
    pub fn get_runtime_config(&self, runtime: &str) -> Result<RuntimeConfig> {
        let socket_path = self
            .config
            .sockets
            .get(runtime)
            .ok_or_else(|| anyhow::anyhow!("Unknown runtime: {}", runtime))?;

        let sandbox_config = self
            .config
            .config_dir
            .join(format!("sandbox-{}.yaml", runtime));
        let container_config = self
            .config
            .config_dir
            .join(format!("container-{}.yaml", runtime));

        Ok(RuntimeConfig {
            name: runtime.to_string(),
            sandbox_config,
            container_config,
            socket_path: socket_path.clone(),
        })
    }

    /// Test a specific runtime end-to-end
    pub async fn test_runtime(&self, runtime: &str) -> Result<TestResult> {
        info!("Testing {} runtime", runtime);

        let runtime_config = self.get_runtime_config(runtime)?;
        let mut test_result = TestResult::new(runtime);

        // Check if the expected Kuasar service socket exists
        if !Path::new(&runtime_config.socket_path).exists() {
            test_result.error = Some(format!(
                "Kuasar service socket not found: {}. Service may not be running.",
                runtime_config.socket_path
            ));
            return Ok(test_result);
        }

        // Create sandbox
        let sandbox_id = match self.create_sandbox(&runtime_config.sandbox_config).await {
            Ok(id) => {
                test_result.sandbox_created = true;
                id
            }
            Err(e) => {
                test_result.error = Some(format!("Failed to create sandbox: {}", e));
                return Ok(test_result);
            }
        };

        // Create container
        let container_id = match self
            .create_container(
                &sandbox_id,
                &runtime_config.container_config,
                &runtime_config.sandbox_config,
            )
            .await
        {
            Ok(id) => {
                test_result.container_created = true;
                id
            }
            Err(e) => {
                self.cleanup_sandbox(&sandbox_id).await.ok();
                test_result.error = Some(format!("Failed to create container: {}", e));
                return Ok(test_result);
            }
        };

        // Start container
        if let Err(e) = self.start_container(&container_id).await {
            self.cleanup_container(&container_id).await.ok();
            self.cleanup_sandbox(&sandbox_id).await.ok();
            test_result.error = Some(format!("Failed to start container: {}", e));
            return Ok(test_result);
        }
        test_result.container_started = true;

        // Wait for container to be running
        if let Err(e) = self
            .wait_for_container_state(&container_id, "CONTAINER_RUNNING")
            .await
        {
            self.cleanup_container(&container_id).await.ok();
            self.cleanup_sandbox(&sandbox_id).await.ok();
            test_result.error = Some(format!("Container failed to reach running state: {}", e));
            return Ok(test_result);
        }
        test_result.container_running = true;

        // Stop container
        if let Err(e) = self.stop_container(&container_id).await {
            warn!("Failed to stop container gracefully: {}", e);
        }

        // Wait for container to be stopped
        if let Err(e) = self
            .wait_for_container_state(&container_id, "CONTAINER_EXITED")
            .await
        {
            warn!("Container may not have stopped gracefully: {}", e);
        } else {
            test_result.container_stopped = true;
        }

        // Cleanup
        self.cleanup_container(&container_id).await.ok();
        self.cleanup_sandbox(&sandbox_id).await.ok();
        test_result.cleanup_completed = true;

        test_result.success = test_result.error.is_none();
        info!(
            "Runtime {} test completed: {:?}",
            runtime, test_result.success
        );

        Ok(test_result)
    }

    /// Explicit cleanup method for services and processes
    pub async fn cleanup(&mut self) -> Result<()> {
        if !self.services_started {
            return Ok(());
        }

        info!("Cleaning up Kuasar services...");

        // First, try to gracefully terminate the child process (local-up-kuasar.sh)
        // This allows the script's trap handlers to perform proper cleanup
        if let Some(mut child) = self.child_process.take() {
            // Get the process ID to send SIGTERM first
            if let Some(pid) = child.id() {
                // Send SIGTERM to allow graceful shutdown via trap handlers
                let _ = TokioCommand::new("kill")
                    .args(&["-TERM", &pid.to_string()])
                    .output()
                    .await;

                // Wait for the process to exit gracefully (with timeout)
                match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                    Ok(Ok(status)) => {
                        debug!("Child process exited gracefully with status: {}", status);
                    }
                    Ok(Err(e)) => {
                        warn!("Error waiting for child process: {}", e);
                        // Force kill if graceful shutdown failed
                        let _ = child.kill().await;
                    }
                    Err(_) => {
                        warn!("Child process did not exit within timeout, force killing");
                        // Force kill if timeout exceeded
                        let _ = child.kill().await;
                    }
                }
            } else {
                // If we can't get PID, just force kill
                let _ = child.kill().await;
            }
        }

        // Additional cleanup: force kill any remaining sandboxer processes
        // This is a fallback in case the script's cleanup didn't work
        let sandboxer_processes = ["runc-sandboxer", "wasm-sandboxer", "quark-sandboxer"];
        for process in &sandboxer_processes {
            let _ = TokioCommand::new("pkill")
                .args(&["-f", process])
                .output()
                .await;
        }

        // Clean up socket files and directories that might be left behind
        let _ = TokioCommand::new("sudo")
            .args(&[
                "rm",
                "-rf",
                "/run/kuasar-runc",
                "/run/kuasar-wasm",
                "/run/kuasar-runc.sock",
                "/run/kuasar-wasm.sock",
            ])
            .output()
            .await;

        self.services_started = false;
        info!("Cleanup completed");

        Ok(())
    }
}

/// Result of a runtime test
#[derive(Debug, Clone)]
pub struct TestResult {
    pub runtime: String,
    pub success: bool,
    pub sandbox_created: bool,
    pub container_created: bool,
    pub container_started: bool,
    pub container_running: bool,
    pub container_stopped: bool,
    pub cleanup_completed: bool,
    pub error: Option<String>,
}

impl TestResult {
    fn new(runtime: &str) -> Self {
        Self {
            runtime: runtime.to_string(),
            success: false,
            sandbox_created: false,
            container_created: false,
            container_started: false,
            container_running: false,
            container_stopped: false,
            cleanup_completed: false,
            error: None,
        }
    }
}

impl Drop for E2EContext {
    fn drop(&mut self) {
        if self.services_started {
            // Warn if services weren't properly cleaned up
            // We can't do async cleanup in Drop, so this is just a warning
            eprintln!("Warning: E2EContext dropped without calling cleanup(). Services may still be running.");
        }
    }
}

// CRI operations implementation
impl E2EContext {
    async fn create_sandbox(&self, config_path: &Path) -> Result<String> {
        let output = TokioCommand::new("crictl")
            .args(&["runp", config_path.to_str().unwrap()])
            .output()
            .await
            .context("Failed to execute crictl runp")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create sandbox: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let sandbox_id = String::from_utf8(output.stdout)
            .context("Invalid UTF-8 in sandbox ID")?
            .trim()
            .to_string();

        Ok(sandbox_id)
    }

    async fn create_container(
        &self,
        sandbox_id: &str,
        container_config: &Path,
        sandbox_config: &Path,
    ) -> Result<String> {
        let output = TokioCommand::new("crictl")
            .args(&[
                "create",
                "--with-pull",
                sandbox_id,
                container_config.to_str().unwrap(),
                sandbox_config.to_str().unwrap(),
            ])
            .output()
            .await
            .context("Failed to execute crictl create")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to create container: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let container_id = String::from_utf8(output.stdout)
            .context("Invalid UTF-8 in container ID")?
            .trim()
            .to_string();

        Ok(container_id)
    }

    async fn start_container(&self, container_id: &str) -> Result<()> {
        let output = TokioCommand::new("crictl")
            .args(&["start", container_id])
            .output()
            .await
            .context("Failed to execute crictl start")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to start container: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    async fn stop_container(&self, container_id: &str) -> Result<()> {
        let output = TokioCommand::new("crictl")
            .args(&["stop", container_id])
            .output()
            .await
            .context("Failed to execute crictl stop")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to stop container: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    async fn wait_for_container_state(
        &self,
        container_id: &str,
        expected_state: &str,
    ) -> Result<()> {
        let start_time = Instant::now();

        while start_time.elapsed() < Duration::from_secs(60) {
            match self.get_container_state(container_id).await {
                Ok(state) if state == expected_state => return Ok(()),
                Ok(state) => {
                    debug!(
                        "Container {} state: {} (waiting for {})",
                        container_id, state, expected_state
                    );
                }
                Err(e) => {
                    debug!("Failed to get container state: {}", e);
                }
            }

            sleep(Duration::from_secs(2)).await;
        }

        anyhow::bail!(
            "Container failed to reach state {} within timeout",
            expected_state
        )
    }

    async fn get_container_state(&self, container_id: &str) -> Result<String> {
        let output = TokioCommand::new("crictl")
            .args(&["inspect", container_id])
            .output()
            .await
            .context("Failed to execute crictl inspect")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to inspect container: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let json: Value = serde_json::from_slice(&output.stdout)
            .context("Failed to parse crictl inspect output")?;

        let state = json["status"]["state"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing state in container inspect output"))?;

        Ok(state.to_string())
    }

    async fn cleanup_container(&self, container_id: &str) -> Result<()> {
        let _ = TokioCommand::new("crictl")
            .args(&["rm", "--force", container_id])
            .output()
            .await;
        Ok(())
    }

    async fn cleanup_sandbox(&self, sandbox_id: &str) -> Result<()> {
        let _ = TokioCommand::new("crictl")
            .args(["rmp", "--force", sandbox_id])
            .output()
            .await;
        Ok(())
    }
}

/// Find the root directory of the Kuasar repository
pub fn find_repo_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().context("Failed to get current directory")?;

    // Try up to 10 levels up to find the repository root
    for _ in 0..10 {
        let hack_dir = dir.join("hack");
        let local_up_script = hack_dir.join("local-up-kuasar.sh");

        if local_up_script.exists() {
            return Ok(dir);
        }

        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }

    // If we can't find it by traversing up, try some common patterns
    // This helps in CI environments where the working directory might vary
    let current = std::env::current_dir().context("Failed to get current directory")?;

    // Try if we're in tests/e2e subdirectory
    if current.ends_with("tests/e2e") {
        if let Some(parent) = current.parent() {
            if let Some(repo_root) = parent.parent() {
                let hack_script = repo_root.join("hack").join("local-up-kuasar.sh");
                if hack_script.exists() {
                    return Ok(repo_root.to_path_buf());
                }
            }
        }
    }

    // Try relative to current directory
    let candidates = [
        current.join("../.."), // From tests/e2e
        current.join(".."),    // From tests
        current.clone(),       // Current dir
    ];

    for candidate in &candidates {
        let hack_script = candidate.join("hack").join("local-up-kuasar.sh");
        if hack_script.exists() {
            return candidate
                .canonicalize()
                .context("Failed to canonicalize path");
        }
    }

    anyhow::bail!(
        "Could not find repository root containing hack/local-up-kuasar.sh. Current dir: {:?}",
        current
    )
}

#[cfg(test)]
mod tests;
