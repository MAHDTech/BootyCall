use bootycall_log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::time::Duration;

use bootycall_core::config::Config;
use bootycall_core::state::{HostStatus, StateStore};

use crate::wire::{
    OP_RRQ, OP_WRQ, is_error_packet, make_data_packet, make_error_packet, make_oack_packet,
    parse_ack_packet,
};

#[derive(Debug)]
struct RrqRequest {
    filename: String,
    mode: String,
    blksize: Option<usize>,
    timeout: Option<u64>,
    tsize_requested: bool,
}

fn parse_rrq(packet: &[u8]) -> Option<RrqRequest> {
    if packet.len() < 4 {
        return None;
    }
    let opcode = u16::from_be_bytes([packet[0], packet[1]]);
    if opcode != OP_RRQ {
        return None;
    }

    let mut parts = Vec::new();
    let mut current = Vec::new();
    for &b in &packet[2..] {
        if b == 0 {
            parts.push(current);
            current = Vec::new();
        } else {
            current.push(b);
        }
    }
    if parts.len() < 2 {
        return None;
    }

    let filename = String::from_utf8(parts[0].clone()).ok()?;
    let mode = String::from_utf8(parts[1].clone()).ok()?;

    let mut blksize = None;
    let mut timeout = None;
    let mut tsize_requested = false;

    let mut i = 2;
    while i + 1 < parts.len() {
        let key = String::from_utf8(parts[i].clone()).ok()?.to_lowercase();
        let val = String::from_utf8(parts[i + 1].clone()).ok()?;
        match key.as_str() {
            "blksize" => {
                if let Ok(val_parsed) = val.parse::<usize>() {
                    blksize = Some(val_parsed);
                }
            }
            "timeout" => {
                if let Ok(val_parsed) = val.parse::<u64>() {
                    timeout = Some(val_parsed);
                }
            }
            "tsize" => {
                tsize_requested = true;
            }
            _ => {}
        }
        i += 2;
    }

    Some(RrqRequest {
        filename,
        mode,
        blksize,
        timeout,
        tsize_requested,
    })
}

async fn handle_tftp_transfer(
    socket: UdpSocket,
    client_addr: SocketAddr,
    file_path: PathBuf,
    request: RrqRequest,
    state_store: StateStore,
    mac_addr: Option<String>,
) -> Result<(), std::io::Error> {
    let transfer_start = std::time::Instant::now();

    // RFC 1350: we only implement `octet` (binary) mode. A `netascii` client
    // would need CR/LF translation, and `mail` is illegal — reject anything
    // that isn't octet with an explicit ERROR instead of silently serving raw
    // octet bytes a strict client would mis-handle.
    if !request.mode.eq_ignore_ascii_case("octet") {
        warn!(
            "Rejecting TFTP transfer to {}: unsupported transfer mode {:?} (octet only)",
            client_addr, request.mode
        );
        let err_pkt = make_error_packet(4, "Illegal TFTP operation (only octet mode supported)");
        let _ = socket.send(&err_pkt).await;
        return Ok(());
    }

    // 1. Open file. Distinguish a genuine 404 from a permissions/FD-exhaustion
    // problem: reporting everything as "File not found" hides the real cause
    // both on the wire and in observability.
    let mut file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            let (code, msg): (u16, &str) = match e.kind() {
                std::io::ErrorKind::NotFound => (1, "File not found"),
                std::io::ErrorKind::PermissionDenied => (2, "Access violation"),
                _ => (0, "File open failed"),
            };
            bootycall_log::event!(
                "tftp_transfer_error",
                file = %file_path.display(),
                error = "file_open_failed",
                kind = %format!("{:?}", e.kind()),
            );
            let err_pkt = make_error_packet(code, msg);
            let _ = socket.send(&err_pkt).await;
            return Err(e);
        }
    };

    // 2. Get file size
    let metadata = file.metadata().await?;
    let file_size = metadata.len();

    // 3. Negotiate options
    let mut options = Vec::new();
    let mut negotiated_blksize = 512;
    let mut negotiated_timeout = 3;

    if let Some(blksize) = request.blksize {
        // Clamp to a safe MTU range
        negotiated_blksize = blksize.clamp(512, 1432);
        options.push(("blksize", negotiated_blksize.to_string()));
    }

    if let Some(timeout) = request.timeout {
        negotiated_timeout = timeout.clamp(1, 10);
        options.push(("timeout", negotiated_timeout.to_string()));
    }

    if request.tsize_requested {
        options.push(("tsize", file_size.to_string()));
    }

    // If options are negotiated, send OACK and wait for ACK 0
    // Any receive that isn't the expected ACK (timeout OR wrong-block ACK OR
    // random garbage) counts against the retry budget. Without this a
    // malicious/broken client can wedge us in a busy loop by flooding
    // wrong-block ACKs.
    const MAX_RETRIES: u32 = 5;
    // Overall per-transfer deadline so a slow drip of wrong-block ACKs can't
    // keep the transfer alive indefinitely.
    const TRANSFER_DEADLINE: Duration = Duration::from_secs(120);
    let retry_policy = RetryPolicy {
        per_try_timeout: Duration::from_secs(negotiated_timeout),
        max_retries: MAX_RETRIES,
        deadline: std::time::Instant::now() + TRANSFER_DEADLINE,
    };

    if !options.is_empty() {
        let oack_pkt = make_oack_packet(&options);
        match send_and_await_ack(
            &socket,
            &oack_pkt,
            0,
            retry_policy,
            client_addr,
            "OACK negotiation",
        )
        .await
        {
            Ok(AckOutcome::Acked) => {}
            Ok(AckOutcome::ClientError) => {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "client aborted during OACK negotiation",
                );
                return Ok(());
            }
            Ok(AckOutcome::GaveUp) => {
                error!(
                    "OACK negotiation with {} timed out after max retries",
                    client_addr
                );
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "OACK negotiation timed out",
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "OACK negotiation timed out",
                ));
            }
            Err(e) => {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "I/O error during OACK negotiation",
                );
                return Err(e);
            }
        }
    }

    // 4. Send DATA blocks
    let mut block_num: u16 = 1;
    let mut finished = false;
    let mut read_buf = vec![0u8; negotiated_blksize];
    let mut current_block_data;

    // Read first block. Fill the buffer via a fill loop so a short read
    // (which is legal for AsyncRead) is NOT mistaken for EOF — only a
    // genuine `0` from `read()` marks the end.
    let (bytes_read, hit_eof) = read_fill(&mut file, &mut read_buf).await?;
    current_block_data = read_buf[..bytes_read].to_vec();
    finished = finished || hit_eof || bytes_read < negotiated_blksize;

    loop {
        let data_pkt = make_data_packet(block_num, &current_block_data);
        match send_and_await_ack(
            &socket,
            &data_pkt,
            block_num,
            retry_policy,
            client_addr,
            "data transfer",
        )
        .await
        {
            Ok(AckOutcome::Acked) => {}
            Ok(AckOutcome::ClientError) => {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "client aborted during data transfer",
                );
                return Ok(());
            }
            Ok(AckOutcome::GaveUp) => {
                error!(
                    "TFTP transfer to {} timed out waiting for ACK block {}",
                    client_addr, block_num
                );
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "timed out waiting for data ACK",
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "TFTP block ack timed out",
                ));
            }
            Err(e) => {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "I/O error during data transfer",
                );
                return Err(e);
            }
        }

        if finished {
            info!(
                "TFTP transfer to {} completed successfully ({} blocks sent)",
                client_addr, block_num
            );
            if let Some(ref mac) = mac_addr {
                state_store.update_host_status(mac, HostStatus::Completed, None, None, None, None);
                state_store.log_event(
                    "INFO",
                    Some(mac),
                    &format!("TFTP transfer completed: {} blocks sent", block_num),
                );
            }
            bootycall_log::event!(
                "tftp_transfer_complete",
                mac = mac_addr.as_deref().unwrap_or(""),
                file = %file_path.display(),
                bytes = file_size,
                blocks = block_num,
                duration_ms = transfer_start.elapsed().as_millis() as u64,
            );
            break;
        }

        // Read next block
        block_num = block_num.wrapping_add(1);
        let (bytes_read, hit_eof) = read_fill(&mut file, &mut read_buf).await?;
        current_block_data = read_buf[..bytes_read].to_vec();
        finished = hit_eof || bytes_read < negotiated_blksize;
    }

    Ok(())
}

/// Result of sending a packet and waiting for the matching ACK.
enum AckOutcome {
    /// The expected block number was acknowledged.
    Acked,
    /// The client sent a TFTP ERROR packet — abort the transfer cleanly.
    ClientError,
    /// The retry budget or the overall deadline was exhausted.
    GaveUp,
}

/// Retry-bounding parameters shared by every send-and-await-ack step of a
/// single transfer: identical for OACK negotiation and every DATA block.
#[derive(Clone, Copy)]
struct RetryPolicy {
    /// How long to wait for one ACK before counting a retry.
    per_try_timeout: Duration,
    /// Maximum number of retries before giving up.
    max_retries: u32,
    /// Overall per-transfer deadline; retries stop once it passes.
    deadline: std::time::Instant,
}

/// Send `packet` and wait for an ACK of `expected_block`, retrying on timeout
/// or unexpected packets until either `max_retries` is reached or `deadline`
/// passes.
///
/// Any receive that isn't the expected ACK (timeout, wrong-block ACK, or
/// random garbage) counts against the retry budget so a malicious/broken
/// client cannot wedge us in a busy loop by flooding wrong-block ACKs. A
/// client-sent ERROR packet aborts immediately with [`AckOutcome::ClientError`].
/// Genuine send/recv I/O errors are propagated to the caller.
async fn send_and_await_ack(
    socket: &UdpSocket,
    packet: &[u8],
    expected_block: u16,
    policy: RetryPolicy,
    client_addr: SocketAddr,
    what: &str,
) -> std::io::Result<AckOutcome> {
    let mut retries: u32 = 0;

    while retries < policy.max_retries && std::time::Instant::now() < policy.deadline {
        if let Err(e) = socket.send(packet).await {
            error!("Failed to send during {} to {}: {:?}", what, client_addr, e);
            return Err(e);
        }

        let mut ack_buf = [0u8; 1024];
        match tokio::time::timeout(policy.per_try_timeout, socket.recv(&mut ack_buf)).await {
            Ok(Ok(n)) => {
                let rec = &ack_buf[..n];
                if is_error_packet(rec) {
                    warn!(
                        "Received TFTP error from client {} during {}",
                        client_addr, what
                    );
                    return Ok(AckOutcome::ClientError);
                }
                if let Some(ack_block) = parse_ack_packet(rec)
                    && ack_block == expected_block
                {
                    return Ok(AckOutcome::Acked);
                }
                // Anything else (wrong block, wrong opcode, garbage) —
                // count it toward the retry bound, not just timeouts.
                debug!(
                    "Unexpected packet during {} from {}, retrying...",
                    what, client_addr
                );
                retries += 1;
            }
            Ok(Err(e)) => {
                error!(
                    "Error receiving ACK from {} during {}: {:?}",
                    client_addr, what, e
                );
                return Err(e);
            }
            Err(_) => {
                debug!("Timeout during {} from {}, retrying...", what, client_addr);
                retries += 1;
            }
        }
    }

    Ok(AckOutcome::GaveUp)
}

/// Mark a host's boot as failed and emit a `tftp_transfer_failed` event.
///
/// Called from every TFTP failure path (retry/deadline give-up, a client-sent
/// ERROR packet, or a send/recv I/O error) so a stalled host moves out of
/// `Booting` into `Failed` instead of being stuck there forever, and the
/// status API / dashboard can surface the failed boot.
fn mark_tftp_failed(
    state_store: &StateStore,
    mac_addr: &Option<String>,
    file_path: &std::path::Path,
    reason: &str,
) {
    if let Some(mac) = mac_addr {
        state_store.update_host_status(mac, HostStatus::Failed, None, None, None, None);
        state_store.log_event(
            "ERROR",
            Some(mac),
            &format!("TFTP transfer failed: {}", reason),
        );
    }
    bootycall_log::event!(
        "tftp_transfer_failed",
        mac = mac_addr.as_deref().unwrap_or(""),
        file = %file_path.display(),
        reason = reason,
    );
}

/// Fill `buf` from `file`, looping over multiple `read` calls if needed.
/// Returns `(bytes_read, hit_eof)`. `hit_eof` is true only when a `read`
/// call returned `0` — that's the only reliable EOF signal from AsyncRead.
/// A short-but-nonzero read from the underlying reader used to be treated
/// as EOF and truncated the served file mid-transfer.
async fn read_fill(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<(usize, bool)> {
    let mut total = 0usize;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).await?;
        if n == 0 {
            return Ok((total, true));
        }
        total += n;
    }
    Ok((total, false))
}

/// Ceiling on simultaneous TFTP transfers. Each transfer spawns a task and
/// opens a fresh UDP socket; without a bound a bogus RRQ flood exhausts
/// sockets and file descriptors. RFC 1350 transfers are one-shot boot
/// artefacts, so a modest limit is plenty for real workloads.
const MAX_CONCURRENT_TRANSFERS: usize = 128;

/// Runs the Asynchronous TFTP server UDP loop, serving files from the tftp_root.
pub async fn run_tftp_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(bind_addr).await?;
    info!("TFTP Server listening on {}", bind_addr);
    let transfer_slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRANSFERS));

    let mut buf = [0u8; 1500];
    loop {
        let (len, src_addr) = match socket.recv_from(&mut buf).await {
            Ok(res) => res,
            Err(e) => {
                error!("Failed to receive UDP packet on TFTP port: {:?}", e);
                continue;
            }
        };

        let packet = &buf[..len];
        // Distinguish "not a valid RRQ" from "RRQ we can serve": WRQ and
        // unknown opcodes deserve a proper TFTP ERROR reply so a bad client
        // sees why it failed instead of silently timing out.
        if packet.len() >= 2 {
            let opcode = u16::from_be_bytes([packet[0], packet[1]]);
            match opcode {
                OP_RRQ => {} // RRQ — normal path below
                OP_WRQ => {
                    // WRQ — writes are not supported; RFC 1350 error code 4.
                    let err = make_error_packet(4, "Illegal TFTP operation (WRQ not supported)");
                    let _ = socket.send_to(&err, src_addr).await;
                    continue;
                }
                _ => {
                    // Any other opcode (DATA/ACK/OACK sent to the listener,
                    // OACK from a client, etc.) — illegal on this socket.
                    let err = make_error_packet(4, "Illegal TFTP operation");
                    let _ = socket.send_to(&err, src_addr).await;
                    continue;
                }
            }
        }

        let request = match parse_rrq(packet) {
            Some(req) => req,
            None => {
                continue;
            }
        };

        let client_ip_str = match src_addr {
            SocketAddr::V4(addr) => addr.ip().to_string(),
            SocketAddr::V6(addr) => addr.ip().to_string(),
        };

        let filename = request.filename.clone();

        let (resolved_file_path, mac_addr) = {
            let config_guard = config.read();
            let state_hosts = state_store.list_hosts();
            let host_state = state_hosts
                .iter()
                .find(|h| h.client_ip.as_deref() == Some(&client_ip_str));

            let mut mac_addr = None;
            let mut final_filename = filename.clone();

            if let Some(hs) = host_state {
                mac_addr = Some(hs.mac.clone());
                if let Some(custom_bootloader) = config_guard
                    .find_host(&hs.mac)
                    .and_then(|h| h.bootloader.as_ref())
                {
                    let is_default_request = filename
                        == config_guard.server.default_bootloader_amd64
                        || filename == config_guard.server.default_bootloader_arm64;
                    if is_default_request {
                        info!(
                            "Redirecting default bootloader request to custom override: {} for host {}",
                            custom_bootloader, hs.mac
                        );
                        final_filename = custom_bootloader.clone();
                    }
                }
            }

            let safe_path =
                bootycall_core::safe_join(&config_guard.server.tftp_root, &final_filename);
            (safe_path, mac_addr)
        };

        let file_path = match resolved_file_path {
            Some(path) => path,
            None => {
                warn!(
                    "Rejected TFTP path traversal request: {} from {}",
                    filename, src_addr
                );
                let transfer_socket = match UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to bind transfer socket: {:?}", e);
                        continue;
                    }
                };
                let _ = transfer_socket.connect(src_addr).await;
                let err_pkt = make_error_packet(2, "Access violation (path traversal)");
                let _ = transfer_socket.send(&err_pkt).await;
                continue;
            }
        };

        if let Some(ref mac) = mac_addr {
            state_store.update_host_status(mac, HostStatus::Booting, None, None, None, None);
            state_store.log_event(
                "INFO",
                Some(mac),
                &format!(
                    "Starting TFTP bootloader transfer: {} (mapped to {})",
                    filename,
                    file_path.display()
                ),
            );
        }

        let transfer_socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "Failed to bind transfer socket for client {}: {:?}",
                    src_addr, e
                );
                continue;
            }
        };

        if let Err(e) = transfer_socket.connect(src_addr).await {
            error!(
                "Failed to connect transfer socket to client {}: {:?}",
                src_addr, e
            );
            continue;
        }

        // Bound concurrent transfers via a semaphore permit held for the
        // lifetime of the spawned task. A flood of RRQs then fails fast
        // (client sees a TFTP ERROR "Server busy") instead of exhausting
        // sockets and file descriptors.
        let permit = match transfer_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    "Refusing TFTP transfer to {}: {} concurrent transfers already in flight",
                    src_addr, MAX_CONCURRENT_TRANSFERS
                );
                let err = make_error_packet(0, "Server busy");
                let _ = transfer_socket.send(&err).await;
                continue;
            }
        };

        let transfer_state_store = state_store.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tftp_transfer(
                transfer_socket,
                src_addr,
                file_path,
                request,
                transfer_state_store,
                mac_addr,
            )
            .await
            {
                debug!(
                    "TFTP transfer to {} encountered an error: {:?}",
                    src_addr, e
                );
            }
            drop(permit);
        });
    }
}
