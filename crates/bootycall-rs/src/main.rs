use anyhow::Context;
use bootycall_log::{error, info};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use bootycall_core::config::{Config, watch_config};
use bootycall_core::state::StateStore;

#[derive(Parser, Debug)]
#[command(
    name = "bootycall-rs",
    version = "0.1.0",
    about = "UEFI PXE Server Suite"
)]
struct Cli {
    #[arg(short, long, default_value = "bootycall.yaml")]
    config: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Test OLED rendering with custom text, size, and layout
    OledTest {
        /// Font size in points. For built-in fonts, choose 12 (small) or 16 (large). For custom TTF/OTF fonts, choose a value between 6 and 60.
        #[arg(long, default_value = "12")]
        size: usize,
        /// Layout alignment. Options: left, center, right, left-top, left-bottom, center-top, center-bottom, right-top, right-bottom
        #[arg(long, default_value = "left")]
        alignment: String,
        /// Text string to display on the screen
        #[arg(long, default_value = "Hello World")]
        text: String,
        /// Path to a custom TrueType (.ttf/.otf) font file for dynamic rendering
        #[arg(long)]
        font: Option<String>,
    },
    /// Test LED control
    LedTest {
        /// LED color: blue, white, off
        #[arg(long, default_value = "blue")]
        color: String,
        /// Enable blinking loop for 10 seconds (flag only, e.g. --blinking)
        #[arg(long)]
        blinking: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // 1. Initialise logging
    bootycall_log::init();

    // Register panic hook to turn LED Solid White on panic
    std::panic::set_hook(Box::new(|info| {
        bootycall_log::error!("Panic occurred: {:?}", info);
        bootycall_led::activate_white_led();
    }));

    // 2. Parse CLI arguments
    let args = Cli::parse();

    // If subcommands are passed, run them immediately and exit
    if let Some(cmd) = args.command {
        match cmd {
            Commands::OledTest {
                size,
                alignment,
                text,
                font,
            } => {
                info!(
                    "Running OLED text preview test... (Note: stop the bootycall service to prevent overwriting)"
                );

                // Validate size based on custom font vs built-in font
                if font.is_some() {
                    if !(6..=60).contains(&size) {
                        return Err(anyhow::anyhow!(
                            "Error: Custom font size {} is out of range. Must be between 6 and 60.",
                            size
                        ));
                    }
                } else if size != 12 && size != 16 {
                    return Err(anyhow::anyhow!(
                        "Error: Built-in font size {} is not supported. Use 12 (small) or 16 (large).",
                        size
                    ));
                }

                // Validate alignment format
                let valid_aligns = [
                    "left",
                    "center",
                    "right",
                    "left-top",
                    "left-bottom",
                    "center-top",
                    "center-bottom",
                    "right-top",
                    "right-bottom",
                ];
                if !valid_aligns.contains(&alignment.as_str()) {
                    return Err(anyhow::anyhow!(
                        "Error: Invalid alignment '{}'. Valid options are: left, center, right, left-top, left-bottom, center-top, center-bottom, right-top, right-bottom",
                        alignment
                    ));
                }

                bootycall_oled::oled_test(size, &alignment, &text, font.as_deref())?;
                return Ok(());
            }
            Commands::LedTest { color, blinking } => {
                info!(
                    "Running LED test... (Note: stop the bootycall service to prevent overwriting)"
                );
                bootycall_led::led_test(&color, blinking)?;
                return Ok(());
            }
        }
    }

    info!("BootyCall starting up...");

    let (led_stop_tx, led_stop_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        bootycall_led::run_boot_blink(led_stop_rx).await;
    });

    // 2. Parse CLI arguments
    let args = Cli::parse();
    let config_path = PathBuf::from(&args.config);
    if !config_path.exists() {
        return Err(anyhow::anyhow!(
            "Configuration file not found: {}",
            args.config
        ));
    }

    // 3. Load configuration
    let config = Config::load(&config_path).context("Failed to load configuration file")?;

    // 4. Initialise extractor cache sync
    info!("Performing initial cache synchronisation...");
    if let Err(e) = bootycall_extractor::sync_all_hosts_cache(&config) {
        error!("Initial cache sync failed: {:?}", e);
    }

    // 5. Setup shared state
    let shared_config = Arc::new(RwLock::new(config.clone()));
    let state_store = StateStore::new();

    // 6. Setup configuration file watcher for live reload
    let config_path_clone = config_path.clone();
    let shared_config_clone = shared_config.clone();
    let _watcher = watch_config(config_path_clone, move |new_config| {
        info!("Configuration file modified! Syncing cache...");
        if let Err(e) = bootycall_extractor::sync_all_hosts_cache(&new_config) {
            error!("Cache sync failed on config reload: {:?}", e);
        }
        let mut guard = shared_config_clone.write().unwrap();
        *guard = new_config;
    })
    .context("Failed to start configuration file watcher")?;

    // Check for systemd socket activation environment variables (informational)
    if std::env::var("LISTEN_FDS").is_ok() {
        info!(
            "Systemd socket activation detected. Binding ports via standard configuration to guarantee safety."
        );
    }

    // 7. Extract bind addresses
    let (dhcp_bind, tftp_bind, http_bind) = {
        let guard = shared_config.read().unwrap();
        (
            guard.server.proxy_dhcp_bind.clone(),
            guard.server.tftp_bind.clone(),
            guard.server.http_bind.clone(),
        )
    };

    // 8. Spawn protocol server tasks
    let dhcp_config = shared_config.clone();
    let dhcp_store = state_store.clone();
    let dhcp_handle = tokio::spawn(async move {
        if let Err(e) = bootycall_dhcp::run_dhcp_server(&dhcp_bind, dhcp_config, dhcp_store).await {
            error!("Proxy DHCP Server encountered a fatal error: {:?}", e);
        }
    });

    let tftp_config = shared_config.clone();
    let tftp_store = state_store.clone();
    let tftp_handle = tokio::spawn(async move {
        if let Err(e) = bootycall_tftp::run_tftp_server(&tftp_bind, tftp_config, tftp_store).await {
            error!("TFTP Server encountered a fatal error: {:?}", e);
        }
    });

    let http_config = shared_config.clone();
    let http_store = state_store.clone();
    let http_handle = tokio::spawn(async move {
        if let Err(e) = bootycall_http::run_http_server(&http_bind, http_config, http_store).await {
            error!("HTTP Server encountered a fatal error: {:?}", e);
        }
    });

    // 9. Clean up stale hosts periodically
    let cleaner_store = state_store.clone();
    let cleaner_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            cleaner_store.clean_stale_hosts(300); // Clean hosts not seen in 5 minutes
        }
    });

    let oled_store = state_store.clone();
    let oled_enabled = shared_config.read().unwrap().server.oled_enabled;
    let (oled_shutdown_tx, oled_shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut oled_manager_handle = None;
    if oled_enabled {
        oled_manager_handle = Some(tokio::spawn(async move {
            if let Err(e) = bootycall_oled::run_oled_manager(oled_store, oled_shutdown_rx).await {
                error!("OLED Manager encountered a fatal error: {:?}", e);
            }
        }));
    }

    // 10. Wait for interrupt or termination signal
    // Keep boot blink running for at least 3 seconds so the transition pattern is visible
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    let _ = led_stop_tx.send(()).await;

    // Spawn regular LED manager task after boot blink stops
    let led_store = state_store.clone();
    let (led_shutdown_tx, led_shutdown_rx) = tokio::sync::mpsc::channel(1);
    let led_manager_handle = tokio::spawn(async move {
        bootycall_led::run_led_manager(led_store, led_shutdown_rx).await;
    });

    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received (SIGINT). Cleaning up services...");
            let _ = led_shutdown_tx.send(()).await;
            let _ = oled_shutdown_tx.send(()).await;
            let _ = led_manager_handle.await;
            if let Some(handle) = oled_manager_handle {
                let _ = handle.await;
            }
        }
        _ = async {
            #[cfg(unix)]
            {
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                tokio::time::sleep(tokio::time::Duration::from_secs(315360000)).await; // 10 years
            }
        } => {
            info!("Shutdown signal received (SIGTERM). Cleaning up services...");
            let _ = led_shutdown_tx.send(()).await;
            let _ = oled_shutdown_tx.send(()).await;
            let _ = led_manager_handle.await;
            if let Some(handle) = oled_manager_handle {
                let _ = handle.await;
            }
        }
        _ = dhcp_handle => {
            error!("DHCP Server task exited unexpectedly.");
        }
        _ = tftp_handle => {
            error!("TFTP Server task exited unexpectedly.");
        }
        _ = http_handle => {
            error!("HTTP Server task exited unexpectedly.");
        }
        _ = cleaner_handle => {
            error!("Host state cleaner task exited unexpectedly.");
        }
    }

    info!("BootyCall shutdown complete.");
    Ok(())
}
