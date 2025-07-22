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

//! Kuasar E2E Tests
//! 
//! This module contains end-to-end tests for all Kuasar runtimes.

use crate::{E2EConfig, E2EContext};
use serial_test::serial;
use std::time::Duration;
use tokio::time::timeout;
use tracing::info;

/// Test timeout for individual runtime tests
const TEST_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Setup function for tests - ensures clean environment
async fn setup_test_context() -> E2EContext {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();
    
    let config = E2EConfig::default();
    E2EContext::new_with_config(config).await
        .expect("Failed to create test context")
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_runc_runtime_lifecycle() {
        info!("Starting runc runtime lifecycle test");
        
        let mut ctx = setup_test_context().await;
        
        // Start services
        ctx.start_services(&["runc"]).await
            .expect("Failed to start runc service");
        
        // Run the test with timeout
        let result = timeout(TEST_TIMEOUT, ctx.test_runtime("runc")).await
            .expect("Test timed out")
            .expect("Test execution failed");
        
        assert!(result.success, "runc runtime test failed: {:?}", result.error);
        assert!(result.sandbox_created, "Sandbox should be created");
        assert!(result.container_created, "Container should be created");
        assert!(result.container_started, "Container should be started");
        assert!(result.container_running, "Container should reach running state");
        assert!(result.cleanup_completed, "Cleanup should complete");
        
        info!("runc runtime lifecycle test completed successfully");
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_service_startup_and_readiness() {
        info!("Testing service startup and readiness");
        
        let mut ctx = setup_test_context().await;
        let runtimes = ["runc"];
        
        // Start services and verify they become ready
        ctx.start_services(&runtimes).await
            .expect("Services should start and become ready");
        
        // Verify socket files exist
        for runtime in &runtimes {
            let runtime_config = ctx.get_runtime_config(runtime)
                .expect("Should get runtime config");
            
            let socket_path = std::path::Path::new(&runtime_config.socket_path);
            assert!(socket_path.exists(), 
                "Socket file should exist for runtime {}: {}", 
                runtime, runtime_config.socket_path);
        }
        
        info!("Service startup and readiness test completed successfully");
    }

    #[tokio::test]
    #[serial]
    async fn test_configuration_files() {
        info!("Testing configuration files");
        
        let ctx = setup_test_context().await;
        let runtimes = ["runc"];
        
        for runtime in &runtimes {
            let runtime_config = ctx.get_runtime_config(runtime)
                .expect("Should get runtime config");
            
            assert!(runtime_config.sandbox_config.exists(), 
                "Sandbox config should exist for runtime {}: {:?}", 
                runtime, runtime_config.sandbox_config);
                
            assert!(runtime_config.container_config.exists(),
                "Container config should exist for runtime {}: {:?}",
                runtime, runtime_config.container_config);
        }
        
        info!("Configuration files test completed successfully");
    }

    #[tokio::test] 
    async fn test_e2e_context_creation() {
        info!("Testing E2E context creation");
        
        let ctx = E2EContext::new().await
            .expect("Should create E2E context");
        
        assert!(!ctx.test_id.is_empty(), "Test ID should not be empty");
        assert!(ctx.config.repo_root.exists(), "Repo root should exist");
        assert!(ctx.config.config_dir.exists(), "Config dir should exist");
        
        info!("E2E context creation test completed successfully");
    }
}

#[cfg(test)]
mod error_handling_tests {
    use super::*;

    #[tokio::test]
    async fn test_invalid_runtime() {
        info!("Testing invalid runtime handling");
        
        let ctx = setup_test_context().await;
        
        // Test with invalid runtime name
        let result = ctx.get_runtime_config("invalid-runtime");
        assert!(result.is_err(), "Should fail with invalid runtime name");
        
        info!("Invalid runtime handling test completed successfully");
    }

    #[tokio::test]
    #[serial]
    async fn test_service_not_started() {
        info!("Testing behavior when service is not started");
        
        let ctx = setup_test_context().await;
        // Intentionally not starting services
        
        // Clean up any existing socket files to ensure clean state
        let runtime_config = ctx.get_runtime_config("runc").expect("Should get runc config");
        let socket_path = std::path::Path::new(&runtime_config.socket_path);
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }
        
        let result = timeout(Duration::from_secs(10), ctx.test_runtime("runc")).await
            .expect("Test should complete within timeout")
            .expect("Test execution should not panic");
        
        assert!(!result.success, "Test should fail when service is not started");
        assert!(result.error.is_some(), "Error should be reported");
        
        info!("Service not started test completed successfully");
    }
}
