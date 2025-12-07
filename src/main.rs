//! Long Shot - Native Wayland Scrolling Screenshot Tool
//!
//! A high-performance long screenshot (scrolling capture) application for Linux Wayland.
//!
//! Architecture:
//! - Thread A (Input Monitor): Watches /dev/input for scroll wheel events via evdev
//! - Thread B (Worker): Handles Wayland screencopy and OpenCV image stitching
//! - Thread C (GUI): Layer-shell overlay for always-on-top preview

mod capture;
mod gui;
mod input;
mod overlay;
mod selector;
mod stitch;
mod types;
mod worker;

use anyhow::Result;
use clap::Parser;
use log::{error, info, warn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use input::start_input_thread;
use overlay::run_overlay;
use selector::{select_region, start_border_thread};
use types::Channels;
use worker::start_worker_thread;

/// Long Shot - Wayland Scrolling Screenshot Tool
#[derive(Parser, Debug)]
#[command(name = "long-shot-rs")]
#[command(about = "High-performance scrolling screenshot tool for Wayland")]
#[command(version)]
struct Args {
    /// Output file path (auto-generates if not specified)
    #[arg(short, long)]
    output: Option<PathBuf>,
    
    /// Output directory for auto-named files
    #[arg(short = 'd', long)]
    save_dir: Option<PathBuf>,
    
    /// Command to execute after saving (use {} as placeholder for file path)
    /// Example: --exec "imv {}" or --exec "xdg-open {}"
    #[arg(short = 'e', long)]
    exec: Option<String>,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let args = Args::parse();
    
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("Long Shot - Wayland Scrolling Screenshot");
    info!("=========================================");
    
    if let Some(ref output) = args.output {
        info!("Output file: {:?}", output);
    }
    if let Some(ref save_dir) = args.save_dir {
        info!("Save directory: {:?}", save_dir);
    }

    // Check for Wayland session
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        error!("No Wayland display found. This application requires a Wayland session.");
        eprintln!(
            "\n╔══════════════════════════════════════════════════════════════╗"
        );
        eprintln!(
            "║  ERROR: Wayland display not found.                          ║"
        );
        eprintln!(
            "║                                                              ║"
        );
        eprintln!(
            "║  This application requires a Wayland session.               ║"
        );
        eprintln!(
            "║  Please run from a Wayland compositor (Sway, Hyprland, etc) ║"
        );
        eprintln!(
            "╚══════════════════════════════════════════════════════════════╝\n"
        );
        std::process::exit(1);
    }

    // Step 1: Select region using custom selector
    println!("\n📍 Please drag to select a screen region...\n");
    println!("   Left click and drag to select");
    println!("   Right click to cancel\n");

    let region = match select_region() {
        Ok(Some(r)) => {
            info!(
                "Selected region: {}x{} at ({}, {})",
                r.width, r.height, r.x, r.y
            );
            r
        }
        Ok(None) => {
            info!("Selection cancelled");
            println!("\n❌ Selection cancelled.");
            std::process::exit(0);
        }
        Err(e) => {
            error!("Region selection failed: {}", e);
            eprintln!("\n❌ Region selection failed: {}", e);
            std::process::exit(1);
        }
    };

    println!(
        "\n✅ Region selected: {}x{} at ({}, {})",
        region.width, region.height, region.x, region.y
    );
    println!("📜 Start scrolling in your target window to capture!");
    println!("💡 The preview window will update as you scroll.\n");

    // Create communication channels
    let channels = Channels::new();

    // Shared shutdown flag
    let shutdown = Arc::new(AtomicBool::new(false));

    // Start Thread A: Input Monitor
    let input_handle = {
        let shutdown = shutdown.clone();
        let tx = channels.input_tx.clone();
        start_input_thread(tx, shutdown)
    };

    // Start Thread B: Worker
    let worker_handle = {
        let shutdown = shutdown.clone();
        start_worker_thread(
            region,
            channels.input_rx,
            channels.gui_rx,
            channels.worker_tx,
            shutdown,
        )
    };

    // Start border overlay thread (边框绘制在选区外面，不会被录进去)
    let border_handle = {
        let shutdown = shutdown.clone();
        start_border_thread(region, shutdown)
    };

    // Run Thread C: Layer-shell overlay (on main thread)
    // This blocks until the window is closed
    let overlay_result = run_overlay(
        channels.worker_rx, 
        channels.gui_tx, 
        region,
        args.output,
        args.save_dir,
        args.exec,
    );

    // Signal shutdown
    info!("Overlay closed, initiating shutdown...");
    shutdown.store(true, Ordering::SeqCst);

    // Wait for threads to finish
    if let Err(e) = input_handle.join() {
        warn!("Input thread panicked: {:?}", e);
    }
    if let Err(e) = worker_handle.join() {
        warn!("Worker thread panicked: {:?}", e);
    }
    if let Err(e) = border_handle.join() {
        warn!("Border thread panicked: {:?}", e);
    }

    info!("Shutdown complete");

    if let Err(e) = overlay_result {
        error!("Overlay error: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
