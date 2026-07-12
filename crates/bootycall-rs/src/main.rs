use anyhow::Context;
use bootycall_log::{error, info, warn};
use clap::{Parser, Subcommand};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

use bootycall_core::config::{Config, watch_config};
use bootycall_core::state::StateStore;

/// How often the background task sweeps out stale hosts.
const STALE_HOST_SWEEP_INTERVAL_SECS: u64 = 60;
/// A host not seen for this long is considered stale and evicted. Policy value:
/// long enough to survive a slow PXE boot, short enough to keep the dashboard
/// from showing ghosts.
const STALE_HOST_TTL_SECS: u64 = 300;
/// Minimum time the boot-blink pattern runs so the LED transition is visible
/// before the steady-state manager takes over.
const BOOT_BLINK_MIN_SECS: u64 = 3;
/// Effectively-forever sleep used as the non-Unix stand-in for a SIGTERM wait
/// (~10 years). Unix uses a real signal handler; other platforms just park.
#[cfg(not(unix))]
const NON_UNIX_SIGTERM_PARK_SECS: u64 = 315_360_000;

#[derive(Parser, Debug)]
#[command(name = "bootycall-rs", version, about = "UEFI PXE Server Suite")]
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
        /// Font size in points. Supported range: between 6 and 40.
        #[arg(long, default_value = "12")]
        size: usize,
        /// Layout alignment. Options: left, center, right, left-top, left-bottom, center-top, center-bottom, right-top, right-bottom
        #[arg(long, default_value = "left")]
        alignment: String,
        /// Text string to display on the screen
        #[arg(long, default_value = "Hello World")]
        text: String,
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
    /// Load and validate the configuration file, then exit without starting any
    /// servers or touching hardware (0 = valid, non-zero = invalid).
    CheckConfig,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    // 1. Initialise logging
    bootycall_log::init();

    // Register panic hook to turn LED Solid White on panic. The supervised
    // OLED render thread is excluded from the LED latch: its panics are
    // caught, logged, and restarted by the OLED manager, so they must not
    // signal a whole-box "service stopped" white LED.
    std::panic::set_hook(Box::new(|info| {
        bootycall_log::error!("Panic occurred: {:?}", info);
        if std::thread::current().name() == Some(bootycall_oled::OLED_THREAD_NAME) {
            return;
        }
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
            } => {
                info!(
                    "Running OLED text preview test... (Note: stop the bootycall service to prevent overwriting)"
                );

                // Validate size based on built-in font
                if !(6..=40).contains(&size) {
                    return Err(anyhow::anyhow!(
                        "Error: Built-in font size {} is out of range. Must be between 6 and 40.",
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

                bootycall_oled::oled_test(size, &alignment, &text)?;
                return Ok(());
            }
            Commands::LedTest { color, blinking } => {
                info!(
                    "Running LED test... (Note: stop the bootycall service to prevent overwriting)"
                );
                bootycall_led::led_test(&color, blinking)?;
                return Ok(());
            }
            Commands::CheckConfig => {
                // Load + validate (Config::load runs Config::validate) without
                // spawning servers or touching hardware. Exit 0 on success,
                // non-zero (via the propagated error) on any validation failure
                // — the safe pre-deploy check for the atomic-save workflow.
                let config_path = PathBuf::from(&args.config);
                if !config_path.exists() {
                    return Err(anyhow::anyhow!(
                        "Configuration file not found: {}",
                        args.config
                    ));
                }
                Config::load(&config_path)
                    .with_context(|| format!("Configuration '{}' is invalid", args.config))?;
                println!("OK: configuration '{}' is valid", args.config);
                return Ok(());
            }
        }
    }

    info!("BootyCall starting up...");

    let (led_stop_tx, led_stop_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        bootycall_led::run_boot_blink(led_stop_rx).await;
    });

    let config_path = PathBuf::from(&args.config);
    if !config_path.exists() {
        return Err(fail_startup(anyhow::anyhow!(
            "Configuration file not found: {}",
            args.config
        )));
    }

    // 3. Load configuration
    let config = Config::load(&config_path)
        .context("Failed to load configuration file")
        .map_err(fail_startup)?;

    // 4. Initialise extractor cache sync. Extraction can take minutes on
    //    large ISOs and is fully synchronous, so run it on a blocking pool
    //    thread — otherwise it starves the reactor and the boot-blink LED
    //    freezes during startup.
    info!("Performing initial cache synchronisation...");
    let config_for_sync = config.clone();
    let sync_result = tokio::task::spawn_blocking(move || {
        bootycall_extractor::sync_all_hosts_cache(&config_for_sync)
    })
    .await;
    match sync_result {
        Ok(Ok(summary)) => log_sync_summary(&summary),
        Ok(Err(e)) => error!("Initial cache sync failed to start (cache dir): {:?}", e),
        Err(join_err) => error!("Initial cache sync task join error: {:?}", join_err),
    }

    // 5. Setup shared state
    let shared_config = Arc::new(RwLock::new(config.clone()));
    let state_store = StateStore::new();

    // 6. Setup configuration file watcher for live reload
    let config_path_clone = config_path.clone();
    let shared_config_clone = shared_config.clone();
    let _watcher = watch_config(config_path_clone, move |new_config| {
        info!("Configuration file modified! Syncing cache...");
        match bootycall_extractor::sync_all_hosts_cache(&new_config) {
            Ok(summary) => log_sync_summary(&summary),
            Err(e) => error!("Cache sync failed on config reload (cache dir): {:?}", e),
        }
        let mut guard = shared_config_clone.write();
        *guard = new_config;
    })
    .context("Failed to start configuration file watcher")
    .map_err(fail_startup)?;

    // Check for systemd socket activation environment variables (informational)
    if std::env::var("LISTEN_FDS").is_ok() {
        info!(
            "Systemd socket activation detected. Binding ports via standard configuration to guarantee safety."
        );
    }

    // 7. Extract bind addresses
    let (dhcp_bind, tftp_bind, http_bind) = {
        let guard = shared_config.read();
        (
            guard.server.proxy_dhcp_bind.clone(),
            guard.server.tftp_bind.clone(),
            guard.server.http_bind.clone(),
        )
    };

    // 8. Spawn protocol server tasks. Each returns Result so a fatal error
    //    (bind failure, socket error) propagates through the JoinHandle and
    //    turns into a non-zero exit below — systemd Restart=on-failure then
    //    kicks the unit back to life instead of silently succeeding.
    let dhcp_config = shared_config.clone();
    let dhcp_store = state_store.clone();
    let dhcp_handle = tokio::spawn(async move {
        bootycall_dhcp::run_dhcp_server(&dhcp_bind, dhcp_config, dhcp_store)
            .await
            .map_err(anyhow::Error::from)
    });

    let tftp_config = shared_config.clone();
    let tftp_store = state_store.clone();
    let tftp_handle = tokio::spawn(async move {
        bootycall_tftp::run_tftp_server(&tftp_bind, tftp_config, tftp_store)
            .await
            .map_err(anyhow::Error::from)
    });

    let http_config = shared_config.clone();
    let http_store = state_store.clone();
    let http_handle = tokio::spawn(async move {
        bootycall_http::run_http_server(&http_bind, http_config, http_store)
            .await
            .map_err(anyhow::Error::from)
    });

    // 9. Clean up stale hosts periodically
    let cleaner_store = state_store.clone();
    let cleaner_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                STALE_HOST_SWEEP_INTERVAL_SECS,
            ))
            .await;
            cleaner_store.clean_stale_hosts(STALE_HOST_TTL_SECS);
        }
    });

    let oled_store = state_store.clone();
    let (oled_enabled, oled_brightness) = {
        let guard = shared_config.read();
        (guard.server.oled_enabled, guard.server.oled_brightness)
    };
    let (oled_shutdown_tx, oled_shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut oled_manager_handle = None;
    if oled_enabled {
        oled_manager_handle = Some(tokio::spawn(async move {
            if let Err(e) =
                bootycall_oled::run_oled_manager(oled_store, oled_brightness, oled_shutdown_rx)
                    .await
            {
                error!("OLED Manager encountered a fatal error: {:?}", e);
            }
        }));
    }

    // 10. Wait for interrupt or termination signal
    // Keep boot blink running briefly so the transition pattern is visible.
    tokio::time::sleep(tokio::time::Duration::from_secs(BOOT_BLINK_MIN_SECS)).await;
    let _ = led_stop_tx.send(()).await;

    // Spawn regular LED manager task after boot blink stops
    let led_store = state_store.clone();
    let (led_shutdown_tx, led_shutdown_rx) = tokio::sync::mpsc::channel(1);
    let led_manager_handle = tokio::spawn(async move {
        bootycall_led::run_led_manager(led_store, led_shutdown_rx).await;
    });

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register SIGTERM handler")?;

    // A signal exits cleanly (Ok); a protocol server dying is treated as
    // fatal and returns Err so the process exit code is non-zero.
    let outcome: anyhow::Result<()> = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received (SIGINT). Cleaning up services...");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Ok(())
        }
        _ = async {
            #[cfg(unix)]
            {
                sigterm.recv().await;
            }
            #[cfg(not(unix))]
            {
                tokio::time::sleep(tokio::time::Duration::from_secs(NON_UNIX_SIGTERM_PARK_SECS))
                    .await;
            }
        } => {
            info!("Shutdown signal received (SIGTERM). Cleaning up services...");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Ok(())
        }
        res = dhcp_handle => {
            let err = join_result_to_error("Proxy DHCP", res);
            error!("Proxy DHCP Server exited: {err:?}");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Err(err)
        }
        res = tftp_handle => {
            let err = join_result_to_error("TFTP", res);
            error!("TFTP Server exited: {err:?}");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Err(err)
        }
        res = http_handle => {
            let err = join_result_to_error("HTTP", res);
            error!("HTTP Server exited: {err:?}");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Err(err)
        }
        _ = cleaner_handle => {
            let err = anyhow::anyhow!("Host state cleaner task exited unexpectedly");
            error!("{err}");
            graceful_shutdown(
                &led_shutdown_tx,
                &oled_shutdown_tx,
                led_manager_handle,
                oled_manager_handle,
            )
            .await;
            Err(err)
        }
    };

    info!("BootyCall shutdown complete.");
    outcome
}

/// Signal the LED and OLED managers to stop and await their tasks so the
/// hardware is left in a defined shutdown state rather than mid-render.
///
/// Shared by every shutdown path — the SIGINT/SIGTERM arms and the fatal
/// server-exit arms (issue 007) — so hardware is always blanked before the
/// process exits, whether that exit is clean or fatal.
async fn graceful_shutdown(
    led_shutdown_tx: &tokio::sync::mpsc::Sender<()>,
    oled_shutdown_tx: &tokio::sync::mpsc::Sender<()>,
    led_manager_handle: tokio::task::JoinHandle<()>,
    oled_manager_handle: Option<tokio::task::JoinHandle<()>>,
) {
    let _ = led_shutdown_tx.send(()).await;
    let _ = oled_shutdown_tx.send(()).await;
    let _ = led_manager_handle.await;
    if let Some(handle) = oled_manager_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(6), handle)
            .await
            .is_err();
        if timed_out {
            warn!("OLED manager shutdown timed out");
        }
    }
}

/// Mark a startup failure on the rack LED before an early `Err` return
/// (issue 075).
///
/// The boot-blink task is already running on the config-missing/load/watch
/// failure paths, but returning from `main` tears the runtime down, so that
/// task cannot be relied on to record the failure — it may be cancelled
/// mid-blink, and before issue 075 its closed stop channel even turned the
/// LED solid blue ("Service Running") on a box that failed to start. Write
/// the white "Service Stopped" state synchronously here so the hardware
/// reflects the failure before the process exits; `run_boot_blink` now also
/// maps a dropped stop channel to white, so a racing blink tick can never
/// overwrite this with blue.
fn fail_startup(err: anyhow::Error) -> anyhow::Error {
    bootycall_led::activate_white_led();
    err
}

/// Apply the cache-sync restart policy (issue 020) and log the outcome.
///
/// Config-level faults are already rejected up front by `Config::validate`, so
/// a failure here is a data/image problem, not a systematically bad config.
/// Policy: a partial failure is logged but we keep serving the hosts that did
/// extract; an all-hosts-failed sync is surfaced prominently (it is the signal
/// the `/api/health` probe reflects) but is deliberately NOT turned into a
/// fatal process exit — doing so under `Restart=on-failure` would restart-spin
/// on a condition a restart cannot fix (e.g. an image that is simply absent).
fn log_sync_summary(summary: &bootycall_extractor::SyncSummary) {
    if summary.all_failed() {
        error!(
            "Cache sync: ALL {} host(s) failed extraction — the box will serve no boot artifacts until this is resolved",
            summary.total()
        );
        for (host, e) in &summary.failed {
            error!("  host {host}: {e}");
        }
    } else if summary.partial_failure() {
        error!(
            "Cache sync: {}/{} host(s) failed extraction (continuing to serve the rest)",
            summary.failed.len(),
            summary.total()
        );
        for (host, e) in &summary.failed {
            error!("  host {host}: {e}");
        }
    } else {
        info!("Cache sync: all {} host(s) OK", summary.total());
    }
}

/// Unwrap a spawned server's `JoinHandle` outcome into a single
/// `anyhow::Error`. A task panic, a task cancel, or a returned server
/// error all collapse to a fatal error worth exiting on.
fn join_result_to_error(
    name: &str,
    res: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Error {
    match res {
        Ok(Ok(())) => anyhow::anyhow!("{name} server exited unexpectedly with Ok"),
        Ok(Err(e)) => e.context(format!("{name} server fatal error")),
        Err(join_err) => anyhow::anyhow!("{name} server task join error: {join_err}"),
    }
}
