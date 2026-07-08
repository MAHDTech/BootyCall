use bootycall_log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::UdpSocket;
use tokio::time::Duration;

use bootycall_core::config::Config;
use bootycall_core::state::{HostStatus, StateStore};

#[derive(Debug)]
struct RrqRequest {
    filename: String,
    #[allow(dead_code)]
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
    if opcode != 1 {
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

fn make_error_packet(code: u16, msg: &str) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(5 + msg.len());
    pkt.extend_from_slice(&5u16.to_be_bytes()); // Opcode 5
    pkt.extend_from_slice(&code.to_be_bytes()); // Error code
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    pkt
}

fn make_oack_packet(options: &[(&str, String)]) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&6u16.to_be_bytes()); // Opcode 6
    for (name, val) in options {
        pkt.extend_from_slice(name.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(val.as_bytes());
        pkt.push(0);
    }
    pkt
}

fn make_data_packet(block_num: u16, data: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + data.len());
    pkt.extend_from_slice(&3u16.to_be_bytes()); // Opcode 3
    pkt.extend_from_slice(&block_num.to_be_bytes());
    pkt.extend_from_slice(data);
    pkt
}

fn parse_ack_packet(pkt: &[u8]) -> Option<u16> {
    if pkt.len() < 4 {
        return None;
    }
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    if opcode != 4 {
        return None;
    }
    Some(u16::from_be_bytes([pkt[2], pkt[3]]))
}

fn is_error_packet(pkt: &[u8]) -> bool {
    if pkt.len() < 4 {
        return false;
    }
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    opcode == 5
}

async fn handle_tftp_transfer(
    socket: UdpSocket,
    client_addr: SocketAddr,
    file_path: PathBuf,
    request: RrqRequest,
    state_store: StateStore,
    mac_addr: Option<String>,
) -> Result<(), std::io::Error> {
    // 1. Open file
    let mut file = match tokio::fs::File::open(&file_path).await {
        Ok(f) => f,
        Err(e) => {
            let err_pkt = make_error_packet(1, "File not found");
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
    let deadline = std::time::Instant::now() + TRANSFER_DEADLINE;

    if !options.is_empty() {
        let oack_pkt = make_oack_packet(&options);
        let mut retries: u32 = 0;
        let mut acked = false;

        while retries < MAX_RETRIES && std::time::Instant::now() < deadline {
            if let Err(e) = socket.send(&oack_pkt).await {
                error!("Failed to send OACK to {}: {:?}", client_addr, e);
                return Err(e);
            }

            let mut ack_buf = [0u8; 1024];
            match tokio::time::timeout(
                Duration::from_secs(negotiated_timeout),
                socket.recv(&mut ack_buf),
            )
            .await
            {
                Ok(Ok(n)) => {
                    let rec = &ack_buf[..n];
                    if is_error_packet(rec) {
                        warn!(
                            "Received TFTP error from client {} during option negotiation",
                            client_addr
                        );
                        return Ok(());
                    }
                    if let Some(0) = parse_ack_packet(rec) {
                        acked = true;
                        break;
                    }
                    // Anything else (wrong block, wrong opcode, garbage) —
                    // count it toward the retry bound, not just timeouts.
                    debug!(
                        "Unexpected packet during OACK negotiation from {}, retrying...",
                        client_addr
                    );
                    retries += 1;
                }
                Ok(Err(e)) => {
                    error!("Error receiving OACK ACK from {}: {:?}", client_addr, e);
                    return Err(e);
                }
                Err(_) => {
                    debug!(
                        "Timeout waiting for OACK ACK from {}, retrying...",
                        client_addr
                    );
                    retries += 1;
                }
            }
        }

        if !acked {
            error!(
                "OACK negotiation with {} timed out after max retries",
                client_addr
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "OACK negotiation timed out",
            ));
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
        let mut retries: u32 = 0;
        let mut acked = false;

        while retries < MAX_RETRIES && std::time::Instant::now() < deadline {
            if let Err(e) = socket.send(&data_pkt).await {
                error!(
                    "Failed to send TFTP block {} to {}: {:?}",
                    block_num, client_addr, e
                );
                return Err(e);
            }

            let mut ack_buf = [0u8; 1024];
            match tokio::time::timeout(
                Duration::from_secs(negotiated_timeout),
                socket.recv(&mut ack_buf),
            )
            .await
            {
                Ok(Ok(n)) => {
                    let rec = &ack_buf[..n];
                    if is_error_packet(rec) {
                        warn!(
                            "Received TFTP error from client {} during transfer",
                            client_addr
                        );
                        return Ok(());
                    }
                    if let Some(ack_block) = parse_ack_packet(rec)
                        && ack_block == block_num
                    {
                        acked = true;
                        break;
                    }
                    // Wrong-block ACK, junk, whatever — count toward the
                    // retry bound so a flood can't wedge us in a busy loop.
                    debug!(
                        "Unexpected packet while waiting for ACK block {} from {}, retrying...",
                        block_num, client_addr
                    );
                    retries += 1;
                }
                Ok(Err(e)) => {
                    error!("Error receiving TFTP ACK from {}: {:?}", client_addr, e);
                    return Err(e);
                }
                Err(_) => {
                    debug!(
                        "Timeout waiting for ACK block {} from {}, retrying...",
                        block_num, client_addr
                    );
                    retries += 1;
                }
            }
        }

        if !acked {
            error!(
                "TFTP transfer to {} timed out waiting for ACK block {}",
                client_addr, block_num
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "TFTP block ack timed out",
            ));
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

/// Runs the Asynchronous TFTP server UDP loop, serving files from the tftp_root.
pub async fn run_tftp_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(bind_addr).await?;
    info!("TFTP Server listening on {}", bind_addr);

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
                1 => {} // RRQ — normal path below
                2 => {
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
        });
    }
}
