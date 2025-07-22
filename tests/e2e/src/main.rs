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

//! Kuasar End-to-End Test Runner
//! 
//! This binary runs comprehensive end-to-end tests for Kuasar runtimes.

use anyhow::{Context, Result};
use kuasar_e2e::{E2EConfig, E2EContext, TestResult};
use std::collections::HashMap;
use std::env;
use std::time::Instant;
use tracing::{error, info};
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    
    let args: Vec<String> = env::args().collect();
    let mut runtimes = vec!["runc"];
    let mut parallel = false;
    
    // Parse command line arguments
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--runtime" | "-r" => {
                if i + 1 < args.len() {
                    runtimes = args[i + 1].split(',').map(|s| s.trim()).collect();
                    i += 2;
                } else {
                    eprintln!("--runtime requires a value");
                    std::process::exit(1);
                }
            }
            "--parallel" | "-p" => {
                parallel = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                print_help();
                std::process::exit(1);
            }
        }
    }
    
    info!("Starting Kuasar E2E tests");
    info!("Runtimes to test: {:?}", runtimes);
    info!("Parallel execution: {}", parallel);
    
    let start_time = Instant::now();
    let config = E2EConfig::default();
    
    // Create E2E context and start services
    let mut ctx = E2EContext::new_with_config(config).await
        .context("Failed to create E2E context")?;
    
    ctx.start_services(&runtimes.iter().map(|s| *s).collect::<Vec<_>>()).await
        .context("Failed to start Kuasar services")?;
    
    // Run tests
    let results = if parallel {
        run_tests_parallel(&ctx, &runtimes).await?
    } else {
        run_tests_sequential(&ctx, &runtimes).await?
    };
    
    // Print results
    let total_duration = start_time.elapsed();
    print_results(&results, total_duration);
    
    // Exit with appropriate code
    let failed_tests = results.values().filter(|r| !r.success).count();
    if failed_tests > 0 {
        error!("{} tests failed", failed_tests);
        std::process::exit(1);
    } else {
        info!("All tests passed!");
        Ok(())
    }
}

async fn run_tests_sequential(ctx: &E2EContext, runtimes: &[&str]) -> Result<HashMap<String, TestResult>> {
    let mut results = HashMap::new();
    
    for runtime in runtimes {
        info!("Running test for runtime: {}", runtime);
        let result = ctx.test_runtime(runtime).await
            .unwrap_or_else(|e| {
                TestResult {
                    runtime: runtime.to_string(),
                    success: false,
                    sandbox_created: false,
                    container_created: false,
                    container_started: false,
                    container_running: false,
                    container_stopped: false,
                    cleanup_completed: false,
                    error: Some(e.to_string()),
                }
            });
        
        results.insert(runtime.to_string(), result);
    }
    
    Ok(results)
}

async fn run_tests_parallel(ctx: &E2EContext, runtimes: &[&str]) -> Result<HashMap<String, TestResult>> {
    let mut handles = Vec::new();
    
    for runtime in runtimes {
        let runtime_name = runtime.to_string();
        let ctx_clone = unsafe { std::mem::transmute::<&E2EContext, &'static E2EContext>(ctx) };
        
        let handle = tokio::spawn(async move {
            info!("Running test for runtime: {}", runtime_name);
            let result = ctx_clone.test_runtime(&runtime_name).await
                .unwrap_or_else(|e| {
                    TestResult {
                        runtime: runtime_name.clone(),
                        success: false,
                        sandbox_created: false,
                        container_created: false,
                        container_started: false,
                        container_running: false,
                        container_stopped: false,
                        cleanup_completed: false,
                        error: Some(e.to_string()),
                    }
                });
            
            (runtime_name, result)
        });
        
        handles.push(handle);
    }
    
    let mut results = HashMap::new();
    for handle in handles {
        let (runtime, result) = handle.await
            .context("Failed to join test task")?;
        results.insert(runtime, result);
    }
    
    Ok(results)
}

fn print_results(results: &HashMap<String, TestResult>, total_duration: std::time::Duration) {
    println!("\n=== Kuasar E2E Test Results ===");
    println!("Total test duration: {:.2}s\n", total_duration.as_secs_f64());
    
    let mut passed = 0;
    let mut failed = 0;
    
    for (runtime, result) in results {
        let status = if result.success { "PASS" } else { "FAIL" };
        let status_symbol = if result.success { "✓" } else { "✗" };
        
        println!("{} {} Runtime: {}", status_symbol, status, runtime);
        
        if result.success {
            println!("  ✓ Sandbox created");
            println!("  ✓ Container created");
            println!("  ✓ Container started");
            println!("  ✓ Container running");
            println!("  ✓ Container stopped");
            println!("  ✓ Cleanup completed");
            passed += 1;
        } else {
            // Show detailed failure information
            println!("  {} Sandbox created: {}", 
                if result.sandbox_created { "✓" } else { "✗" }, 
                result.sandbox_created);
            println!("  {} Container created: {}", 
                if result.container_created { "✓" } else { "✗" }, 
                result.container_created);
            println!("  {} Container started: {}", 
                if result.container_started { "✓" } else { "✗" }, 
                result.container_started);
            println!("  {} Container running: {}", 
                if result.container_running { "✓" } else { "✗" }, 
                result.container_running);
            println!("  {} Container stopped: {}", 
                if result.container_stopped { "✓" } else { "✗" }, 
                result.container_stopped);
            println!("  {} Cleanup completed: {}", 
                if result.cleanup_completed { "✓" } else { "✗" }, 
                result.cleanup_completed);
                
            if let Some(error) = &result.error {
                println!("  Error: {}", error);
            }
            failed += 1;
        }
        println!();
    }
    
    println!("Summary: {} passed, {} failed", passed, failed);
}

fn print_help() {
    println!("Kuasar E2E Test Runner\n");
    println!("Usage: kuasar-e2e [OPTIONS]\n");
    println!("Options:");
    println!("  -r, --runtime <RUNTIMES>  Comma-separated list of runtimes to test");
    println!("                            Available: runc");
    println!("                            Default: runc");
    println!("  -p, --parallel            Run tests in parallel");
    println!("  -h, --help                Show this help message\n");
    println!("Environment variables:");
    println!("  ARTIFACTS                 Directory to store test artifacts");
    println!("  KUASAR_LOG_LEVEL         Log level (default: info)");
    println!("  RUST_LOG                  Tracing log filter");
}
