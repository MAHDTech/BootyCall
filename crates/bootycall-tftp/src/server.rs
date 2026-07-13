use bootycall_log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::time::Duration;

use bootycall_core::config::Config;
use bootycall_core::state::{HostStatus, StateStore};

use crate::error::TftpError;
use crate::wire::{
    OP_RRQ, OP_WRQ, is_error_packet, make_data_packet, make_error_packet, make_oack_packet,
    parse_ack_packet,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq)]
struct RrqRequest {
    filename: String,
    mode: String,
    blksize: Option<usize>,
    timeout: Option<u64>,
    tsize_requested: bool,
    windowsize: Option<u16>,
}

#[tracing::instrument(skip_all, fields(packet_len = packet.len()))]
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
    let mut windowsize = None;

    let mut i = 2;
    while i + 1 < parts.len() {
        // RFC 2347: options the server cannot parse are ignored, not fatal.
        // A single non-UTF-8 key/value pair used to `?`-fail the whole RRQ,
        // silently dropping an otherwise-valid request — skip the pair and
        // keep negotiating the rest instead.
        let (Ok(key), Ok(val)) = (
            std::str::from_utf8(&parts[i]),
            std::str::from_utf8(&parts[i + 1]),
        ) else {
            debug!("Ignoring non-UTF-8 TFTP option pair at index {}", i);
            i += 2;
            continue;
        };
        let key = key.to_lowercase();
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
            "windowsize" => {
                if let Ok(val_parsed) = val.parse::<u16>() {
                    windowsize = Some(val_parsed);
                }
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
        windowsize,
    })
}

/// Per-transfer span: every log and `bootycall_log::event!` record emitted
/// while a bootloader streams out carries the client address, MAC, and file
/// path. The socket/state-store/request args are skipped (non-Debug or
/// noisy); the useful request fields are recorded explicitly.
#[tracing::instrument(
    skip_all,
    fields(
        %client_addr,
        mac = mac_addr.as_deref().unwrap_or(""),
        file = %file_path.display(),
        filename = %request.filename,
    )
)]
async fn handle_tftp_transfer(
    socket: UdpSocket,
    client_addr: SocketAddr,
    file_path: PathBuf,
    request: RrqRequest,
    state_store: StateStore,
    mac_addr: Option<String>,
) -> Result<(), TftpError> {
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
            return Err(e.into());
        }
    };

    // 2. Get file size
    let metadata = file.metadata().await?;
    let file_size = metadata.len();

    // 3. Negotiate options
    let mut options = Vec::new();
    let mut negotiated_blksize = DEFAULT_BLKSIZE;
    let mut negotiated_timeout = DEFAULT_TIMEOUT_SECS;
    let mut negotiated_windowsize: u16 = 1;

    if let Some(blksize) = request.blksize {
        // Clamp to a safe MTU range and ensure we never exceed the requested size.
        // Keep the RFC minimum of 8 as the floor.
        negotiated_blksize = blksize.clamp(8, MAX_BLKSIZE);
        options.push(("blksize", negotiated_blksize.to_string()));
    }

    if let Some(timeout) = request.timeout {
        negotiated_timeout = timeout.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
        options.push(("timeout", negotiated_timeout.to_string()));
    }

    if request.tsize_requested {
        options.push(("tsize", file_size.to_string()));
    }

    if let Some(windowsize) = request.windowsize {
        // RFC 7440: a window of N blocks per ACK. Clamp to [1, MAX_WINDOWSIZE]
        // so a client cannot force us to buffer an unbounded window in memory.
        negotiated_windowsize = windowsize.clamp(1, MAX_WINDOWSIZE);
        options.push(("windowsize", negotiated_windowsize.to_string()));
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
                return Err(TftpError::OackNegotiationTimedOut);
            }
            Err(e) => {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "I/O error during OACK negotiation",
                );
                return Err(e.into());
            }
        }
    }

    // 4. Send DATA blocks with a sliding window (RFC 7440), go-back-N on loss.
    //    A negotiated window of 1 is ordinary stop-and-wait. The window's blocks
    //    are buffered so a retransmission never re-reads the file, and reads are
    //    strictly forward (a rollback replays from the buffer).
    let window = negotiated_windowsize as u64;
    let mut read_buf = vec![0u8; negotiated_blksize];
    // Buffered window blocks keyed by absolute (1-indexed) block number.
    let mut buffered: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
    let mut base: u64 = 1; // first unacknowledged block
    let mut next_to_send: u64 = 1; // next block to transmit
    let mut last_block: Option<u64> = None; // absolute number of the final (short) block
    let mut retries: u32 = 0;
    let mut ack_buf = [0u8; 1024];

    // TFTP block numbers are 16-bit and start at 1, wrapping 65535 -> 0.
    let wire_of = |abs: u64| -> u16 { (abs % 65_536) as u16 };

    // Send a DATA packet, marking the host failed on an I/O error before
    // propagating it.
    macro_rules! send_data_or_fail {
        ($abs:expr, $data:expr) => {
            if let Err(e) = socket.send(&make_data_packet(wire_of($abs), $data)).await {
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "I/O error during data transfer",
                );
                return Err(e.into());
            }
        };
    }

    loop {
        // Fill the window: transmit every block in [base, base + window) not yet
        // sent, reading forward from the file. Retransmits reuse the buffer, so
        // `read_fill` only ever runs on brand-new blocks — a short read (legal
        // for AsyncRead) is NOT mistaken for EOF, only a genuine `0` marks it.
        while next_to_send < base + window {
            if let Some(lb) = last_block
                && next_to_send > lb
            {
                break; // nothing past the terminating block
            }
            let data = match buffered.get(&next_to_send) {
                Some(d) => d.clone(),
                None => {
                    let (bytes_read, hit_eof) = read_fill(&mut file, &mut read_buf).await?;
                    let d = read_buf[..bytes_read].to_vec();
                    if hit_eof || bytes_read < negotiated_blksize {
                        last_block = Some(next_to_send);
                    }
                    buffered.insert(next_to_send, d.clone());
                    d
                }
            };
            send_data_or_fail!(next_to_send, &data);
            next_to_send += 1;
        }

        // Bound the wait: retry budget and the overall transfer deadline. Any
        // receive that isn't a window-advancing ACK (timeout, old/dup ACK, or
        // garbage) counts against the retry budget so a broken/malicious client
        // cannot wedge us in a busy loop.
        //
        // Mitigation for optionless RRQ amplification/reflection attack (B5):
        // If the OACK handshake was skipped (meaning options list was empty),
        // we have not yet established that the client's source IP is not spoofed.
        // To prevent amplification, we limit the retransmissions of the first block
        // (before any ACK from the client is received, i.e., base == 1) to at most
        // 1 retry instead of the standard max_retries (5). Once any ACK is received
        // (base > 1), the client's address is verified to be responsive, and we
        // can use the full retry budget.
        let max_allowed_retries = if options.is_empty() && base == 1 {
            1
        } else {
            retry_policy.max_retries
        };

        if retries >= max_allowed_retries || std::time::Instant::now() >= retry_policy.deadline {
            error!(
                "TFTP transfer to {} timed out waiting for ACK (base block {})",
                client_addr, base
            );
            mark_tftp_failed(
                &state_store,
                &mac_addr,
                &file_path,
                "timed out waiting for data ACK",
            );
            return Err(TftpError::BlockAckTimedOut);
        }

        match tokio::time::timeout(retry_policy.per_try_timeout, socket.recv(&mut ack_buf)).await {
            Ok(Ok(n)) => {
                let rec = &ack_buf[..n];
                if is_error_packet(rec) {
                    warn!(
                        "Received TFTP error from client {} during data transfer",
                        client_addr
                    );
                    mark_tftp_failed(
                        &state_store,
                        &mac_addr,
                        &file_path,
                        "client aborted during data transfer",
                    );
                    return Ok(());
                }
                match parse_ack_packet(rec) {
                    Some(acked_wire) => {
                        // Map the wire ACK to an absolute block in the in-flight
                        // range [base, next_to_send). Only an ACK for a block we
                        // actually sent advances the window.
                        let acked_abs =
                            (base..next_to_send).find(|&abs| wire_of(abs) == acked_wire);
                        match acked_abs {
                            Some(abs) => {
                                // Cumulative ACK: retire blocks up to `abs`.
                                base = abs + 1;
                                buffered.retain(|&k, _| k >= base);
                                retries = 0;
                                if let Some(lb) = last_block
                                    && base > lb
                                {
                                    break; // whole file acknowledged
                                }
                            }
                            None => {
                                // Old/duplicate ACK or garbage.
                                debug!(
                                    "Unexpected ACK block {} from {}, retrying...",
                                    acked_wire, client_addr
                                );
                                next_to_send = base;
                            }
                        }
                    }
                    None => {
                        debug!(
                            "Unexpected packet during data transfer from {}, retrying...",
                            client_addr
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Error receiving TFTP ACK from {}: {:?}", client_addr, e);
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "I/O error during data transfer",
                );
                return Err(e.into());
            }
            Err(_) => {
                // Timeout — roll back to `base` and resend the whole window.
                debug!(
                    "Timeout waiting for ACK from {}, resending window...",
                    client_addr
                );
                retries += 1;
                next_to_send = base;
            }
        }
    }

    let total_blocks = last_block.unwrap_or(0);
    info!(
        "TFTP transfer to {} completed successfully ({} blocks sent, windowsize {})",
        client_addr, total_blocks, negotiated_windowsize
    );
    if let Some(ref mac) = mac_addr {
        let _ = state_store.update_host_status(mac, HostStatus::Completed, None, None, None, None);
        let _ = state_store.log_event(
            "INFO",
            Some(mac),
            &format!("TFTP transfer completed: {} blocks sent", total_blocks),
        );
    }
    bootycall_log::event!(
        "tftp_transfer_complete",
        mac = mac_addr.as_deref().unwrap_or(""),
        file = %file_path.display(),
        bytes = file_size,
        blocks = total_blocks,
        windowsize = negotiated_windowsize,
        duration_ms = transfer_start.elapsed().as_millis() as u64,
    );

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
#[tracing::instrument(skip_all, fields(%client_addr, expected_block, what))]
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
                // do not count it toward the retry bound, not just timeouts.
                debug!(
                    "Unexpected packet during {} from {}, retrying...",
                    what, client_addr
                );
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
/// Called from every TFTP failure path — retry/deadline give-up, a client-sent
/// ERROR packet, a send/recv I/O error, and the transfer setup failures in the
/// server loop (socket bind/connect errors and the "Server busy" semaphore
/// rejection) — so a stalled or rejected host moves out of `Booting` into
/// `Failed` instead of being stuck there forever, and the status API /
/// dashboard can surface the failed boot.
fn mark_tftp_failed(
    state_store: &StateStore,
    mac_addr: &Option<String>,
    file_path: &std::path::Path,
    reason: &str,
) {
    if let Some(mac) = mac_addr {
        let _ = state_store.update_host_status(mac, HostStatus::Failed, None, None, None, None);
        let _ = state_store.log_event(
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

/// Upper bound on the negotiated RFC 7440 window size. The in-flight window is
/// buffered in memory (`window * blksize` bytes per transfer), so this caps a
/// client's ability to force us to buffer unboundedly.
const MAX_WINDOWSIZE: u16 = 32;

/// RFC 1350 default TFTP block size (also the initial value before any `blksize`
/// option is seen).
const DEFAULT_BLKSIZE: usize = 512;
/// Upper bound on a negotiated `blksize`. Kept under the common 1500-byte
/// Ethernet MTU (less the 20-byte IP + 8-byte UDP + 4-byte TFTP headers) so a
/// single DATA packet is not IP-fragmented on a standard LAN.
const MAX_BLKSIZE: usize = 1432;

/// RFC 2349 default per-packet retransmission timeout, in seconds — the value
/// used before any `timeout` option is negotiated.
const DEFAULT_TIMEOUT_SECS: u64 = 3;
/// Clamp range, in seconds, for a client-requested `timeout` option.
const MIN_TIMEOUT_SECS: u64 = 1;
const MAX_TIMEOUT_SECS: u64 = 10;

/// Wildcard bind address matching the peer's address family.
///
/// The per-transfer and reject sockets must share the client's address
/// family: a socket bound to IPv4 `0.0.0.0:0` cannot `connect` to an IPv6
/// peer (address-family mismatch), which previously broke IPv6 clients
/// accepted by a `[::]`-bound listener — the RRQ never transferred and the
/// client timed out.
fn wildcard_bind_addr(peer: &SocketAddr) -> &'static str {
    if peer.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
}

/// Runs the Asynchronous TFTP server UDP loop, serving files from the tftp_root.
///
/// Concurrent transfers are bounded by [`MAX_CONCURRENT_TRANSFERS`]. Use
/// [`run_tftp_server_with_limit`] to override the bound (tests inject a small
/// limit to exercise the "Server busy" rejection).
///
/// Span layout: the listener loop carries one process-lifetime
/// `run_tftp_server_with_limit{bind_addr}` span (a span per loop iteration
/// would be meaningless for an accept loop); per-transfer context comes from
/// the [`handle_tftp_transfer`] span on each spawned transfer task.
#[tracing::instrument(skip(config, state_store, shutdown))]
pub async fn run_tftp_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
    shutdown: CancellationToken,
) -> Result<(), TftpError> {
    run_tftp_server_with_limit(
        bind_addr,
        config,
        state_store,
        MAX_CONCURRENT_TRANSFERS,
        shutdown,
    )
    .await
}

/// Like [`run_tftp_server`] but with an explicit concurrent-transfer bound so
/// tests can drive the semaphore-rejection ("Server busy") path at a small
/// limit instead of the production default of 128.
#[tracing::instrument(skip(config, state_store, shutdown))]
pub async fn run_tftp_server_with_limit(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
    max_concurrent_transfers: usize,
    shutdown: CancellationToken,
) -> Result<(), TftpError> {
    let socket = UdpSocket::bind(bind_addr).await?;
    info!("TFTP Server listening on {}", bind_addr);
    let transfer_slots = Arc::new(tokio::sync::Semaphore::new(max_concurrent_transfers));

    let mut buf = [0u8; 1500];
    loop {
        let (len, src_addr) = tokio::select! {
            _ = shutdown.cancelled() => {
                info!("TFTP server listener shutting down gracefully");
                break;
            }
            res = socket.recv_from(&mut buf) => {
                match res {
                    Ok(val) => val,
                    Err(e) => {
                        error!("Failed to receive UDP packet on TFTP port: {:?}", e);
                        continue;
                    }
                }
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
                // Reaching here with >= 2 bytes means the opcode was OP_RRQ
                // (WRQ/unknown opcodes were answered above) but the body is
                // genuinely unparseable — a malformed RRQ. Answer with ERROR
                // code 4 so the client fails fast instead of timing out.
                // Anything shorter than an opcode is random non-TFTP garbage:
                // stay silent rather than reply to arbitrary traffic.
                if packet.len() >= 2 {
                    warn!(
                        "Malformed TFTP RRQ from {} ({} bytes), sending ERROR",
                        src_addr, len
                    );
                    let err = make_error_packet(4, "Illegal TFTP operation (malformed RRQ)");
                    let _ = socket.send_to(&err, src_addr).await;
                }
                continue;
            }
        };

        let client_ip_str = match src_addr {
            SocketAddr::V4(addr) => addr.ip().to_string(),
            SocketAddr::V6(addr) => addr.ip().to_string(),
        };

        let filename = request.filename.clone();

        let (tftp_root, final_filename, mac_addr) = {
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
                    // A request for *any* of the configured defaults — the
                    // legacy-BIOS NBP included, which this check used to
                    // omit — is redirected to the host's per-MAC override.
                    let is_default_request = filename
                        == config_guard.server.default_bootloader_amd64
                        || filename == config_guard.server.default_bootloader_arm64
                        || filename == config_guard.server.default_bootloader_bios;
                    if is_default_request {
                        info!(
                            "Redirecting default bootloader request to custom override: {} for host {}",
                            custom_bootloader, hs.mac
                        );
                        final_filename = custom_bootloader.clone();
                    }
                }
            }

            (
                config_guard.server.tftp_root.clone(),
                final_filename,
                mac_addr,
            )
        };

        // CONVENTION (issue 070): no blocking `std::fs` on the async runtime.
        // `safe_join` canonicalises (two blocking stat/resolve syscalls), so
        // it runs on the blocking pool — the listener is a single task and an
        // inline call would stall every pending RRQ *and* the worker thread.
        // The config read guard above is dropped first: awaiting while
        // holding it would block config reloads for the duration of the
        // filesystem work (parking_lot guards must never live across .await).
        let resolved_file_path = match tokio::task::spawn_blocking(move || {
            bootycall_core::safe_join(&tftp_root, &final_filename)
        })
        .await
        {
            Ok(path) => path,
            Err(e) => {
                // Task panicked or was cancelled — never fail open on a
                // path-containment check; drop the request.
                error!("TFTP path resolution task failed for {}: {:?}", src_addr, e);
                continue;
            }
        };

        let file_path = match resolved_file_path {
            Some(path) => path,
            None => {
                warn!(
                    "Rejected TFTP path traversal request: {} from {}",
                    filename, src_addr
                );
                let err_pkt = make_error_packet(2, "Access violation (path traversal)");
                let _ = socket.send_to(&err_pkt, src_addr).await;
                continue;
            }
        };

        if let Some(ref mac) = mac_addr {
            let _ =
                state_store.update_host_status(mac, HostStatus::Booting, None, None, None, None);
            let _ = state_store.log_event(
                "INFO",
                Some(mac),
                &format!(
                    "Starting TFTP bootloader transfer: {} (mapped to {})",
                    filename,
                    file_path.display()
                ),
            );
        }

        // Bound concurrent transfers via a semaphore permit held for the
        // lifetime of the spawned task. A flood of RRQs then fails fast
        // (client sees a TFTP ERROR "Server busy") instead of exhausting
        // sockets and file descriptors.
        // Acquire the permit *first* before binding/connecting the ephemeral transfer socket.
        let permit = match transfer_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    "Refusing TFTP transfer to {}: {} concurrent transfers already in flight",
                    src_addr, max_concurrent_transfers
                );
                let err = make_error_packet(0, "Server busy");
                let _ = socket.send_to(&err, src_addr).await;
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "transfer rejected: server busy",
                );
                continue;
            }
        };

        // From here on the host is already marked `Booting`, and the permit is held,
        // so every setup failure path must go through `mark_tftp_failed`.
        let transfer_socket = match UdpSocket::bind(wildcard_bind_addr(&src_addr)).await {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "Failed to bind transfer socket for client {}: {:?}",
                    src_addr, e
                );
                mark_tftp_failed(
                    &state_store,
                    &mac_addr,
                    &file_path,
                    "failed to bind transfer socket",
                );
                continue;
            }
        };

        if let Err(e) = transfer_socket.connect(src_addr).await {
            error!(
                "Failed to connect transfer socket to client {}: {:?}",
                src_addr, e
            );
            mark_tftp_failed(
                &state_store,
                &mac_addr,
                &file_path,
                "failed to connect transfer socket",
            );
            continue;
        }

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

    if max_concurrent_transfers > 0 {
        match transfer_slots
            .acquire_many(max_concurrent_transfers as u32)
            .await
        {
            Ok(_permits) => {
                info!("All TFTP transfers completed, shutdown complete");
            }
            Err(e) => {
                error!("Semaphore closed while draining TFTP transfers: {:?}", e);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rrq_too_short() {
        assert!(parse_rrq(&[0, 1]).is_none());
        assert!(parse_rrq(&[0, 1, 0]).is_none());
    }

    #[test]
    fn test_parse_rrq_wrong_opcode() {
        let packet = [
            0, 2, b'f', b'i', b'l', b'e', 0, b'o', b'c', b't', b'e', b't', 0,
        ];
        assert!(parse_rrq(&packet).is_none());
    }

    #[test]
    fn test_parse_rrq_only_filename() {
        let packet = [0, 1, b't', b'e', b's', b't', b'.', b't', b'x', b't', 0];
        assert!(parse_rrq(&packet).is_none());
    }

    #[test]
    fn test_parse_rrq_non_utf8() {
        let packet_bad_file = [0, 1, 0xff, 0xfe, 0, b'o', b'c', b't', b'e', b't', 0];
        assert!(parse_rrq(&packet_bad_file).is_none());

        let packet_bad_mode = [0, 1, b't', b'e', b's', b't', 0, 0xff, 0xfe, 0];
        assert!(parse_rrq(&packet_bad_mode).is_none());
    }

    #[test]
    fn test_parse_rrq_blksize_garbage() {
        let mut packet = vec![0, 1];
        packet.extend_from_slice(b"test.txt\0octet\0");
        packet.extend_from_slice(b"blksize\0abc\0");
        packet.extend_from_slice(b"timeout\05\0");

        let req = parse_rrq(&packet).expect("Expected Some RrqRequest");
        assert_eq!(req.filename, "test.txt");
        assert_eq!(req.mode, "octet");
        assert_eq!(req.blksize, None);
        assert_eq!(req.timeout, Some(5));
    }

    #[test]
    fn test_parse_rrq_netascii_mail_modes() {
        let mut packet = vec![0, 1];
        packet.extend_from_slice(b"test.txt\0netascii\0");
        let req = parse_rrq(&packet).expect("Expected RrqRequest for netascii");
        assert_eq!(req.mode, "netascii");

        let mut packet2 = vec![0, 1];
        packet2.extend_from_slice(b"test.txt\0mail\0");
        let req2 = parse_rrq(&packet2).expect("Expected RrqRequest for mail");
        assert_eq!(req2.mode, "mail");
    }
}
