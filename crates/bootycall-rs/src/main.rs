use anyhow::Context;
use bootycall_log::{error, info, warn};
use clap::{Parser, Subcommand};
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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

                validate_oled_params(size, &alignment)?;

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

    let config_path = PathBuf::from(&args.config);
    if !config_path.exists() {
        return Err(anyhow::anyhow!(
            "Configuration file not found: {}",
            args.config
        ));
    }

    // 3. Load configuration
    let config = Config::load(&config_path).context("Failed to load configuration file")?;

    let led_enabled = config.server.led_enabled;

    let (led_stop_tx, led_stop_rx) = tokio::sync::mpsc::channel(1);
    let mut boot_blink_handle = None;
    if led_enabled {
        boot_blink_handle = Some(tokio::spawn(async move {
            bootycall_led::run_boot_blink(led_stop_rx).await;
        }));
    }

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

    let shutdown_token = CancellationToken::new();

    // 6. Setup configuration file watcher for live reload
    let (config_tx, mut config_rx) = tokio::sync::watch::channel(config.clone());
    let config_tx_clone = config_tx.clone();

    let config_path_clone = config_path.clone();
    let shared_config_clone = shared_config.clone();
    let _watcher = watch_config(config_path_clone, move |new_config| {
        info!("Configuration file modified! Updating configuration guard...");
        {
            let mut guard = shared_config_clone.write();
            *guard = new_config.clone();
        }
        if let Err(e) = config_tx_clone.send(new_config) {
            error!(
                "Failed to send configuration update to background task: {:?}",
                e
            );
        }
    })
    .context("Failed to start configuration file watcher")
    .map_err(|e| fail_startup(e, led_enabled))?;

    let shutdown_token_clone = shutdown_token.clone();
    let sync_lock = Arc::new(tokio::sync::Mutex::new(()));
    let extractor_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_token_clone.cancelled() => {
                    break;
                }
                changed_res = config_rx.changed() => {
                    if changed_res.is_err() {
                        break;
                    }
                    let new_config = {
                        let guard = config_rx.borrow();
                        guard.clone()
                    };
                    let sync_lock_clone = sync_lock.clone();
                    let _permit = sync_lock_clone.lock_owned().await;
                    info!("Configuration file reload: starting background cache sync...");
                    let handle = tokio::task::spawn_blocking(move || {
                        let _permit = _permit;
                        bootycall_extractor::sync_all_hosts_cache(&new_config)
                    });
                    let sync_result = handle.await;
                    match sync_result {
                        Ok(Ok(summary)) => log_sync_summary(&summary),
                        Ok(Err(e)) => error!("Cache sync failed on config reload (cache dir): {:?}", e),
                        Err(join_err) => error!("Cache sync task join error: {:?}", join_err),
                    }
                }
            }
        }
    });

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
    let dhcp_token = shutdown_token.clone();
    let mut dhcp_handle = tokio::spawn(async move {
        bootycall_dhcp::run_dhcp_server(&dhcp_bind, dhcp_config, dhcp_store, dhcp_token)
            .await
            .map_err(anyhow::Error::from)
    });

    let tftp_config = shared_config.clone();
    let tftp_store = state_store.clone();
    let tftp_token = shutdown_token.clone();
    let mut tftp_handle = tokio::spawn(async move {
        bootycall_tftp::run_tftp_server(&tftp_bind, tftp_config, tftp_store, tftp_token)
            .await
            .map_err(anyhow::Error::from)
    });

    let http_config = shared_config.clone();
    let http_store = state_store.clone();
    let http_token = shutdown_token.clone();
    let mut http_handle = tokio::spawn(async move {
        bootycall_http::run_http_server(&http_bind, http_config, http_store, http_token)
            .await
            .map_err(anyhow::Error::from)
    });

    // 9. Clean up stale hosts periodically
    let cleaner_store = state_store.clone();
    let mut cleaner_handle = tokio::spawn(async move {
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
    let (led_shutdown_tx, led_shutdown_rx) = tokio::sync::mpsc::channel(1);

    let boot_blink_timeout =
        tokio::time::sleep(tokio::time::Duration::from_secs(BOOT_BLINK_MIN_SECS));
    tokio::pin!(boot_blink_timeout);

    let early_exit: Option<anyhow::Result<()>> = tokio::select! {
        _ = &mut boot_blink_timeout => {
            None
        }
        res = &mut dhcp_handle => {
            let err = join_result_to_error("Proxy DHCP", res);
            error!("Proxy DHCP Server exited early: {err:?}");
            Some(Err(err))
        }
        res = &mut tftp_handle => {
            let err = join_result_to_error("TFTP", res);
            error!("TFTP Server exited early: {err:?}");
            Some(Err(err))
        }
        res = &mut http_handle => {
            let err = join_result_to_error("HTTP", res);
            error!("HTTP Server exited early: {err:?}");
            Some(Err(err))
        }
        res = async {
            if let Some(ref mut h) = boot_blink_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            boot_blink_handle = None;
            let err = match res {
                Ok(()) => anyhow::anyhow!("Boot blink task exited prematurely"),
                Err(join_err) => anyhow::anyhow!("Boot blink task panicked or failed: {:?}", join_err),
            };
            error!("Boot blink task exited early: {err:?}");
            Some(Err(err))
        }
    };

    if let Some(Err(err)) = early_exit {
        // Shutdown OLED manager if it was started
        graceful_shutdown(
            &shutdown_token,
            &led_shutdown_tx,
            &oled_shutdown_tx,
            None,
            oled_manager_handle.take(),
            Some(dhcp_handle),
            Some(tftp_handle),
            Some(http_handle),
            Some(extractor_handle),
        )
        .await;

        // If boot_blink_handle is still running, signal it to stop and await it
        if let Some(handle) = boot_blink_handle.take() {
            let _ = led_stop_tx.send(()).await;
            let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
            match timed_out {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("Boot blink task join error on early exit: {:?}", e);
                }
                Err(_) => {
                    warn!("Boot blink task join on early exit timed out");
                }
            }
        }

        return Err(fail_startup(err, led_enabled));
    }

    // Stop the boot-blink task and confirm it has completed before spawning led_manager
    let _ = led_stop_tx.send(()).await;
    if let Some(handle) = boot_blink_handle.take() {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match timed_out {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("Boot-blink task join error/panic: {:?}", e);
            }
            Err(_) => {
                warn!("Boot-blink task join timed out during transition");
            }
        }
    }

    // Spawn regular LED manager task after boot blink stops
    let led_store = state_store.clone();
    let mut led_manager_handle = None;
    if led_enabled {
        led_manager_handle = Some(tokio::spawn(async move {
            bootycall_led::run_led_manager(led_store, led_shutdown_rx).await;
        }));
    }

    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register SIGTERM handler")?;

    // A signal exits cleanly (Ok); a protocol server dying is treated as
    // fatal and returns Err so the process exit code is non-zero.
    let outcome: anyhow::Result<()> = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received (SIGINT). Cleaning up services...");
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
            Ok(())
        }
        res = &mut dhcp_handle => {
            let err = join_result_to_error("Proxy DHCP", res);
            error!("Proxy DHCP Server exited: {err:?}");
            Err(err)
        }
        res = &mut tftp_handle => {
            let err = join_result_to_error("TFTP", res);
            error!("TFTP Server exited: {err:?}");
            Err(err)
        }
        res = &mut http_handle => {
            let err = join_result_to_error("HTTP", res);
            error!("HTTP Server exited: {err:?}");
            Err(err)
        }
        _ = &mut cleaner_handle => {
            let err = anyhow::anyhow!("Host state cleaner task exited unexpectedly");
            error!("{err}");
            Err(err)
        }
        res = async {
            if let Some(ref mut h) = led_manager_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            let err = match res {
                Ok(()) => anyhow::anyhow!("LED manager task exited prematurely"),
                Err(join_err) => anyhow::anyhow!("LED manager task panicked or failed: {:?}", join_err),
            };
            error!("{err}");
            Err(err)
        }
        res = async {
            if let Some(ref mut h) = oled_manager_handle {
                h.await
            } else {
                std::future::pending().await
            }
        } => {
            let err = match res {
                Ok(()) => anyhow::anyhow!("OLED manager task exited prematurely"),
                Err(join_err) => anyhow::anyhow!("OLED manager task panicked or failed: {:?}", join_err),
            };
            error!("{err}");
            Err(err)
        }
    };

    graceful_shutdown(
        &shutdown_token,
        &led_shutdown_tx,
        &oled_shutdown_tx,
        led_manager_handle.take(),
        oled_manager_handle.take(),
        Some(dhcp_handle),
        Some(tftp_handle),
        Some(http_handle),
        Some(extractor_handle),
    )
    .await;

    info!("BootyCall shutdown complete.");
    outcome
}

/// Signal the LED and OLED managers to stop and await their tasks so the
/// hardware is left in a defined shutdown state rather than mid-render.
///
/// Shared by every shutdown path — the SIGINT/SIGTERM arms and the fatal
/// server-exit arms (issue 007) — so hardware is always blanked before the
/// process exits, whether that exit is clean or fatal.
#[allow(clippy::too_many_arguments)]
async fn graceful_shutdown(
    shutdown_token: &CancellationToken,
    led_shutdown_tx: &tokio::sync::mpsc::Sender<()>,
    oled_shutdown_tx: &tokio::sync::mpsc::Sender<()>,
    led_manager_handle: Option<tokio::task::JoinHandle<()>>,
    oled_manager_handle: Option<tokio::task::JoinHandle<()>>,
    dhcp_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    tftp_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    http_handle: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>>,
    extractor_handle: Option<tokio::task::JoinHandle<()>>,
) {
    let _ = led_shutdown_tx.send(()).await;
    let _ = oled_shutdown_tx.send(()).await;
    shutdown_token.cancel();

    if let Some(handle) = extractor_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(15), handle).await;
        match timed_out {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("Extractor loop task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("Extractor loop shutdown timed out");
            }
        }
    }

    if let Some(handle) = led_manager_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match timed_out {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("LED manager task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("LED manager shutdown timed out");
            }
        }
    }
    if let Some(handle) = oled_manager_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(6), handle).await;
        match timed_out {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!("OLED manager task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("OLED manager shutdown timed out");
            }
        }
    }

    if let Some(handle) = dhcp_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match timed_out {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                error!("Proxy DHCP server task exited with error: {:?}", e);
            }
            Ok(Err(e)) => {
                error!("Proxy DHCP server task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("Proxy DHCP server shutdown timed out");
            }
        }
    }

    if let Some(handle) = tftp_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match timed_out {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                error!("TFTP server task exited with error: {:?}", e);
            }
            Ok(Err(e)) => {
                error!("TFTP server task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("TFTP server shutdown timed out");
            }
        }
    }

    if let Some(handle) = http_handle {
        let timed_out = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        match timed_out {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                error!("HTTP server task exited with error: {:?}", e);
            }
            Ok(Err(e)) => {
                error!("HTTP server task panicked or had join error: {:?}", e);
            }
            Err(_) => {
                warn!("HTTP server shutdown timed out");
            }
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
fn fail_startup(err: anyhow::Error, led_enabled: bool) -> anyhow::Error {
    if led_enabled {
        bootycall_led::activate_white_led();
    }
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

fn validate_oled_params(size: usize, alignment: &str) -> anyhow::Result<()> {
    if !(6..=40).contains(&size) {
        return Err(anyhow::anyhow!(
            "Error: Built-in font size {} is out of range. Must be between 6 and 40.",
            size
        ));
    }

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
    if !valid_aligns.contains(&alignment) {
        return Err(anyhow::anyhow!(
            "Error: Invalid alignment '{}'. Valid options are: left, center, right, left-top, left-bottom, center-top, center-bottom, right-top, right-bottom",
            alignment
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_oled_params() {
        // Valid params
        assert!(validate_oled_params(12, "center").is_ok());
        assert!(validate_oled_params(6, "left-top").is_ok());
        assert!(validate_oled_params(40, "right-bottom").is_ok());

        // Invalid sizes
        assert!(validate_oled_params(5, "center").is_err());
        assert!(validate_oled_params(41, "center").is_err());

        // Invalid alignments
        assert!(validate_oled_params(12, "invalid-align").is_err());
        assert!(validate_oled_params(12, "").is_err());
    }

    #[test]
    fn test_join_result_to_error() {
        // 1. Success case
        let err_ok = join_result_to_error("Test", Ok(Ok(())));
        assert_eq!(
            err_ok.to_string(),
            "Test server exited unexpectedly with Ok"
        );

        // 2. Fatal error case
        let inner_err = anyhow::anyhow!("database lookup failed");
        let err_fatal = join_result_to_error("Test", Ok(Err(inner_err)));
        assert!(err_fatal.to_string().contains("Test server fatal error"));
    }

    #[tokio::test]
    async fn test_join_result_to_error_panic() {
        // 3. Panic / task join error case
        let handle = tokio::spawn(async {
            panic!("intended panic");
        });
        let res = handle.await;
        assert!(res.is_err());
        let err_panic = join_result_to_error("Test", res);
        assert!(
            err_panic
                .to_string()
                .contains("Test server task join error")
        );
    }

    #[test]
    fn test_log_sync_summary_branches() {
        use bootycall_extractor::SyncSummary;
        use bootycall_extractor::error::ExtractorError;

        // 1. Success (all hosts OK)
        let summary_ok = SyncSummary {
            succeeded: 2,
            failed: vec![],
        };
        log_sync_summary(&summary_ok);

        // 2. Partial failure
        let summary_partial = SyncSummary {
            succeeded: 1,
            failed: vec![(
                "host_failed".to_string(),
                ExtractorError::Io(std::io::Error::new(std::io::ErrorKind::Other, "disk full")),
            )],
        };
        log_sync_summary(&summary_partial);

        // 3. All failed
        let summary_all_failed = SyncSummary {
            succeeded: 0,
            failed: vec![(
                "host_failed_1".to_string(),
                ExtractorError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "network timeout",
                )),
            )],
        };
        log_sync_summary(&summary_all_failed);
    }
}
