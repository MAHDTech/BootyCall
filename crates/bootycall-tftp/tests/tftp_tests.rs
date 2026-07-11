use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use std::fs::{self, File};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::net::UdpSocket;

// Thin adapters over the shared `bootycall_tftp::wire` helpers, so the tests
// no longer keep a parallel copy of the packet layout (issue 027).
fn parse_oack(pkt: &[u8]) -> Vec<(String, String)> {
    bootycall_tftp::wire::parse_oack_packet(pkt).expect("expected an OACK packet")
}

fn parse_data(pkt: &[u8]) -> (u16, Vec<u8>) {
    bootycall_tftp::wire::parse_data_packet(pkt).expect("expected a DATA packet")
}

fn make_rrq_packet(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    bootycall_tftp::wire::make_rrq_packet(filename, "octet", options)
}

fn make_ack_packet(block: u16) -> Vec<u8> {
    bootycall_tftp::wire::make_ack_packet(block)
}

#[tokio::test]
async fn test_tftp_server_negotiation_and_transfer() {
    // 1. Setup temporary directory for tftp root and a test file
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    // Create default bootloader directories and file
    let default_dir = tftp_root.join("boot/x64");
    fs::create_dir_all(&default_dir).unwrap();
    let bootloader_path = default_dir.join("ipxe.efi");
    let file_content = b"This is the default EFI payload!";
    {
        let mut file = File::create(&bootloader_path).unwrap();
        file.write_all(file_content).unwrap();
    }

    // Create a special override file
    let special_dir = tftp_root.join("boot");
    fs::create_dir_all(&special_dir).unwrap();
    let special_path = special_dir.join("special.efi");
    let special_content = b"This is a special custom EFI payload override!";
    {
        let mut file = File::create(&special_path).unwrap();
        file.write_all(special_content).unwrap();
    }

    // 2. Setup server configuration
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "127.0.0.1:25069".to_string(),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    };

    let host = HostConfig {
        mac: "00:aa:bb:cc:dd:ee".to_string(),
        name: "test-client".to_string(),
        image_path: "/tmp/nixos.iso".into(),
        bootloader: Some("boot/special.efi".to_string()),
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    let config = Config {
        server: server_config,
        hosts: vec![host],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Insert host in state store, mapping 127.0.0.1 to MAC 00:aa:bb:cc:dd:ee
    state_store.update_host_status(
        "00:aa:bb:cc:dd:ee",
        HostStatus::Polling,
        Some("test-client".to_string()),
        None,
        Some("127.0.0.1".to_string()),
        Some("x86_64".to_string()),
    );

    // 3. Spawn TFTP Server
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25069", server_config_clone, server_store)
                .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Client socket to send request
    let client_socket = UdpSocket::bind("127.0.0.1:25070").await.unwrap();

    // Client requests the default bootloader, but has a special override
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );

    client_socket
        .send_to(&rrq, "127.0.0.1:25069")
        .await
        .unwrap();

    // 5. Receive option acknowledgement (OACK)
    let mut response_buf = [0u8; 1024];
    let (len, server_tid_addr) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let negotiated_options = parse_oack(&response_buf[..len]);

    // Assert options
    assert!(negotiated_options.contains(&("blksize".to_string(), "512".to_string())));
    assert!(negotiated_options.contains(&("timeout".to_string(), "1".to_string())));
    // tsize should match special_content size since it was redirected to special.efi!
    assert!(negotiated_options.contains(&("tsize".to_string(), special_content.len().to_string())));

    // Check host status in StateStore updated to Booting
    let host_state = state_store.get_host("00:aa:bb:cc:dd:ee").unwrap();
    assert_eq!(host_state.status, HostStatus::Booting);

    // 6. Send ACK for block 0 to Server TID
    let ack0 = make_ack_packet(0);
    client_socket.send_to(&ack0, server_tid_addr).await.unwrap();

    // 7. Receive DATA block 1
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let (block_num, data) = parse_data(&response_buf[..len]);
    assert_eq!(block_num, 1);
    assert_eq!(data, special_content.to_vec());

    // 8. Send ACK for block 1
    let ack1 = make_ack_packet(1);
    client_socket.send_to(&ack1, server_tid_addr).await.unwrap();

    // Wait briefly for TFTP task to finalise and update state store
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Check host status in StateStore is Completed
    let host_state_completed = state_store.get_host("00:aa:bb:cc:dd:ee").unwrap();
    assert_eq!(host_state_completed.status, HostStatus::Completed);
}

fn parse_error_packet(pkt: &[u8]) -> (u16, u16, String) {
    assert!(pkt.len() >= 5, "ERROR packet too short");
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    let error_code = u16::from_be_bytes([pkt[2], pkt[3]]);
    // Message runs from byte 4 to the trailing null (or end)
    let msg_end = pkt[4..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(pkt.len() - 4);
    let msg = String::from_utf8_lossy(&pkt[4..4 + msg_end]).to_string();
    (opcode, error_code, msg)
}

#[tokio::test]
async fn test_tftp_file_not_found() {
    // Setup temporary directory for tftp root with NO test files
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "127.0.0.1:25074".to_string(),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    };

    let config = Config {
        server: server_config,
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Spawn TFTP server on port 25070
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25074", server_config_clone, server_store)
                .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client socket on a unique port
    let client_socket = UdpSocket::bind("127.0.0.1:25075").await.unwrap();

    // Request a file that does not exist
    let rrq = make_rrq_packet("nonexistent/bootloader.efi", &[]);
    client_socket
        .send_to(&rrq, "127.0.0.1:25074")
        .await
        .unwrap();

    // Receive the ERROR packet from the server's transfer socket
    let mut response_buf = [0u8; 1024];
    let (len, _server_tid) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("Timed out waiting for ERROR response")
    .expect("Failed to receive ERROR response");

    let (opcode, error_code, msg) = parse_error_packet(&response_buf[..len]);
    assert_eq!(opcode, 5, "Expected ERROR opcode (5)");
    assert_eq!(error_code, 1, "Expected error code 1 (File Not Found)");
    assert!(
        msg.contains("not found") || msg.contains("Not found"),
        "Error message should mention 'not found', got: {msg}"
    );
}

async fn assert_tftp_traversal_rejected(bind_port: u16, client_port: u16, requested: &str) {
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: format!("127.0.0.1:{bind_port}"),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    };

    let config = Config {
        server: server_config,
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    let bind = format!("127.0.0.1:{bind_port}");
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(&bind, server_config_clone, server_store).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let client_socket = UdpSocket::bind(format!("127.0.0.1:{client_port}"))
        .await
        .unwrap();

    let rrq = make_rrq_packet(requested, &[]);
    client_socket
        .send_to(&rrq, format!("127.0.0.1:{bind_port}"))
        .await
        .unwrap();

    let mut response_buf = [0u8; 1024];
    let (len, _server_tid) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("Timed out waiting for ERROR response")
    .expect("Failed to receive ERROR response");

    let (opcode, error_code, msg) = parse_error_packet(&response_buf[..len]);
    assert_eq!(opcode, 5, "Expected ERROR opcode (5) for {requested:?}");
    assert_eq!(
        error_code, 2,
        "Expected error code 2 (Access Violation) for {requested:?}"
    );
    assert!(
        msg.contains("Access violation"),
        "Error message should mention 'Access violation', got {msg:?} for {requested:?}"
    );
}

#[tokio::test]
async fn test_tftp_path_traversal_parent_dir_blocked() {
    assert_tftp_traversal_rejected(25076, 25077, "../../etc/passwd").await;
}

#[tokio::test]
async fn test_tftp_path_traversal_absolute_blocked() {
    assert_tftp_traversal_rejected(25086, 25087, "/etc/passwd").await;
}

#[tokio::test]
async fn test_tftp_path_traversal_double_slash_absolute_blocked() {
    assert_tftp_traversal_rejected(25096, 25097, "//etc/passwd").await;
}

#[tokio::test]
async fn test_tftp_transfer_non_multiple_blksize_intact() {
    // File whose size is deliberately a non-multiple of the negotiated
    // blksize. The old "any short read == EOF" heuristic could truncate
    // mid-transfer if the underlying reader ever returned a short-but-
    // nonzero read; the fill loop keeps the DATA blocks accurate.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    // 512 blksize + a small tail — the tail block is a short final block,
    // exactly the boundary case where truncation used to bite.
    let blksize = 512usize;
    let payload: Vec<u8> = (0..(blksize + 137)).map(|i| (i % 256) as u8).collect();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), &payload).unwrap();

    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "127.0.0.1:25106".to_string(),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    };
    let config = Config {
        server: server_config,
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25106", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client_socket = UdpSocket::bind("127.0.0.1:25107").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[
            ("blksize", &blksize.to_string()),
            ("timeout", "1"),
            ("tsize", "0"),
        ],
    );
    client_socket
        .send_to(&rrq, "127.0.0.1:25106")
        .await
        .unwrap();

    // Consume the OACK and ack it.
    let mut buf = [0u8; 2048];
    let (n, server_tid) =
        tokio::time::timeout(Duration::from_secs(2), client_socket.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
    let _ = parse_oack(&buf[..n]);
    client_socket
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();

    // Receive DATA blocks, ack each in turn. Reassemble to compare byte-exact.
    let mut received: Vec<u8> = Vec::new();
    let mut expected_block: u16 = 1;
    loop {
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(2), client_socket.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        let (block, data) = parse_data(&buf[..n]);
        assert_eq!(block, expected_block, "block numbering must be sequential");
        received.extend_from_slice(&data);
        client_socket
            .send_to(&make_ack_packet(block), from)
            .await
            .unwrap();
        // A DATA block strictly shorter than blksize marks the last one.
        if data.len() < blksize {
            break;
        }
        expected_block = expected_block.wrapping_add(1);
    }

    assert_eq!(
        received.len(),
        payload.len(),
        "received {} bytes but source was {}",
        received.len(),
        payload.len()
    );
    assert_eq!(received, payload, "reassembled bytes must equal source");
}

#[tokio::test]
async fn test_tftp_timeout_marks_host_failed() {
    // A client that never sends the expected ACK must cause the server to give
    // up and move the host from `Booting` to `Failed` (issue 009), instead of
    // leaving it stuck in `Booting` forever. We burn the retry budget quickly
    // by flooding wrong-block ACKs (each counts as a retry) so the test does
    // not have to wait out the full per-try timeouts.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    fs::write(
        tftp_root.join("boot/x64/ipxe.efi"),
        b"payload-that-never-gets-acked",
    )
    .unwrap();

    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "127.0.0.1:25120".to_string(),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    };
    let config = Config {
        server: server_config,
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map 127.0.0.1 -> a known MAC so the transfer associates a host.
    let mac = "00:11:22:33:44:55";
    state_store.update_host_status(
        mac,
        HostStatus::Polling,
        Some("failing-client".to_string()),
        None,
        Some("127.0.0.1".to_string()),
        Some("x86_64".to_string()),
    );

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25120", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client_socket = UdpSocket::bind("127.0.0.1:25121").await.unwrap();
    // Negotiate options with a short per-try timeout so the server enters the
    // OACK-ack wait loop.
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );
    client_socket
        .send_to(&rrq, "127.0.0.1:25120")
        .await
        .unwrap();

    // Receive the OACK to learn the server's transfer TID.
    let mut buf = [0u8; 1024];
    let (_n, server_tid) =
        tokio::time::timeout(Duration::from_secs(2), client_socket.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();

    // Never send the expected ACK 0. Flood wrong-block ACKs so each counts
    // against the retry budget and the server gives up quickly.
    for _ in 0..8 {
        let _ = client_socket
            .send_to(&make_ack_packet(9999), server_tid)
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // The server's give-up is asynchronous; poll until the host is Failed.
    let mut failed = false;
    for _ in 0..40 {
        if let Some(hs) = state_store.get_host(mac)
            && hs.status == HostStatus::Failed
        {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        failed,
        "host should be marked Failed after the transfer gives up, got {:?}",
        state_store.get_host(mac).map(|h| h.status)
    );
}

fn base_server_config(bind: &str, tftp_root: &std::path::Path) -> ServerConfig {
    ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: bind.to_string(),
        tftp_root: tftp_root.to_path_buf(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    }
}

#[tokio::test]
async fn test_tftp_server_busy_at_injected_limit() {
    // With the concurrency limit injected down to 1, a second concurrent RRQ
    // must be rejected with ERROR code 0 "Server busy" while the first
    // transfer still holds the only permit.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), b"busy-test-payload").unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25130", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server_with_limit(
            "127.0.0.1:25130",
            server_config_clone,
            server_store,
            1,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );

    // First client: start a transfer and hold the single permit by receiving
    // the OACK but never sending an ACK — the transfer task stays alive
    // (retrying for ACK 0) and keeps the semaphore permit for its lifetime.
    let client_a = UdpSocket::bind("127.0.0.1:25131").await.unwrap();
    client_a.send_to(&rrq, "127.0.0.1:25130").await.unwrap();
    let mut buf = [0u8; 1024];
    let (_n, _tid) = tokio::time::timeout(Duration::from_secs(2), client_a.recv_from(&mut buf))
        .await
        .expect("first client should receive an OACK")
        .unwrap();

    // Second client: its RRQ must be rejected with "Server busy".
    let client_b = UdpSocket::bind("127.0.0.1:25132").await.unwrap();
    client_b.send_to(&rrq, "127.0.0.1:25130").await.unwrap();
    let (n, _from) = tokio::time::timeout(Duration::from_secs(2), client_b.recv_from(&mut buf))
        .await
        .expect("second client should receive a Server busy ERROR")
        .unwrap();
    let (opcode, code, msg) = parse_error_packet(&buf[..n]);
    assert_eq!(opcode, 5, "expected a TFTP ERROR packet (opcode 5)");
    assert_eq!(code, 0, "Server busy uses error code 0");
    assert_eq!(msg, "Server busy");
}

#[tokio::test]
async fn test_tftp_server_busy_marks_known_host_failed() {
    // A known host whose RRQ is rejected with "Server busy" (semaphore
    // exhausted) must end up `Failed`, not stuck in `Booting` (issue 071):
    // the server-loop reject path never reaches `handle_tftp_transfer`, so it
    // has to invoke `mark_tftp_failed` itself.
    //
    // Loopback caveat: hosts are looked up by client IP only, so both client
    // sockets (127.0.0.1, differing ports) map to the same host entry —
    // client B's reject updates the state that client A's request set to
    // `Booting`. That is expected in this environment; the assertion is that
    // the host's final state is `Failed`.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), b"busy-failed-payload").unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25170", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map 127.0.0.1 -> a known MAC so the RRQs associate a host.
    let mac = "00:aa:bb:cc:dd:71";
    state_store.update_host_status(
        mac,
        HostStatus::Polling,
        Some("busy-client".to_string()),
        None,
        Some("127.0.0.1".to_string()),
        Some("x86_64".to_string()),
    );

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server_with_limit(
            "127.0.0.1:25170",
            server_config_clone,
            server_store,
            1,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );

    // First client: start a transfer and hold the single permit by receiving
    // the OACK but never sending an ACK. The host is now `Booting`.
    let client_a = UdpSocket::bind("127.0.0.1:25171").await.unwrap();
    client_a.send_to(&rrq, "127.0.0.1:25170").await.unwrap();
    let mut buf = [0u8; 1024];
    let (_n, _tid) = tokio::time::timeout(Duration::from_secs(2), client_a.recv_from(&mut buf))
        .await
        .expect("first client should receive an OACK")
        .unwrap();
    assert_eq!(
        state_store.get_host(mac).map(|h| h.status),
        Some(HostStatus::Booting),
        "host should be Booting while the first transfer holds the permit"
    );

    // Second client (semaphore exhausted): its RRQ must be rejected with
    // "Server busy" ...
    let client_b = UdpSocket::bind("127.0.0.1:25172").await.unwrap();
    client_b.send_to(&rrq, "127.0.0.1:25170").await.unwrap();
    let (n, _from) = tokio::time::timeout(Duration::from_secs(2), client_b.recv_from(&mut buf))
        .await
        .expect("second client should receive a Server busy ERROR")
        .unwrap();
    let (opcode, code, msg) = parse_error_packet(&buf[..n]);
    assert_eq!(opcode, 5, "expected a TFTP ERROR packet (opcode 5)");
    assert_eq!(code, 0, "Server busy uses error code 0");
    assert_eq!(msg, "Server busy");

    // ... and the known host must end `Failed`, not linger in `Booting`.
    let mut failed = false;
    for _ in 0..40 {
        if let Some(hs) = state_store.get_host(mac)
            && hs.status == HostStatus::Failed
        {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        failed,
        "host should be marked Failed after the Server busy reject, got {:?}",
        state_store.get_host(mac).map(|h| h.status)
    );
}

#[tokio::test]
async fn test_tftp_wrq_rejected_with_error() {
    // A WRQ (opcode 2) is not supported: the listener must reply with an
    // ERROR packet (RFC 1350 code 4, illegal operation) rather than ignore it.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    let config = Config {
        server: base_server_config("127.0.0.1:25140", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25140", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Hand-build a WRQ: opcode 2, filename\0, mode\0.
    let mut wrq = vec![0x00, 0x02];
    wrq.extend_from_slice(b"boot/x64/ipxe.efi");
    wrq.push(0);
    wrq.extend_from_slice(b"octet");
    wrq.push(0);

    let client = UdpSocket::bind("127.0.0.1:25141").await.unwrap();
    client.send_to(&wrq, "127.0.0.1:25140").await.unwrap();

    let mut buf = [0u8; 1024];
    let (n, _from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("WRQ should draw an ERROR reply")
        .unwrap();
    let (opcode, code, _msg) = parse_error_packet(&buf[..n]);
    assert_eq!(opcode, 5, "expected a TFTP ERROR packet (opcode 5)");
    assert_eq!(code, 4, "WRQ rejection uses illegal-operation code 4");
}

/// A windowed (RFC 7440) TFTP receiver used by the windowsize tests.
///
/// Receives DATA blocks, ACKing the last in-order block whenever the window
/// fills (`windowsize` blocks) or the final short block arrives. If the next
/// block does not arrive in time it ACKs the last in-order block anyway, which
/// nudges the server on a gap. `drop_block`, when set, discards that block
/// exactly once to simulate a mid-window loss and force a go-back-N recovery.
/// Returns the reassembled bytes.
async fn windowed_recv_all(
    client: &UdpSocket,
    server_tid: SocketAddr,
    blksize: usize,
    windowsize: u16,
    drop_block: Option<u16>,
) -> Vec<u8> {
    let mut received = Vec::new();
    let mut expected: u64 = 1;
    let mut count: u16 = 0;
    let mut last_in_order: u16 = 0;
    let mut dropped = false;
    let mut buf = vec![0u8; blksize + 64];
    loop {
        match tokio::time::timeout(Duration::from_millis(400), client.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let (block, data) = parse_data(&buf[..n]);
                // Simulate a single lost block: drop it once, then accept the
                // retransmission.
                if drop_block == Some(block) && !dropped {
                    dropped = true;
                    continue;
                }
                // Discard out-of-order blocks (e.g. those after a dropped one).
                if block as u64 != expected {
                    continue;
                }
                received.extend_from_slice(&data);
                last_in_order = block;
                expected += 1;
                count += 1;
                let is_short = data.len() < blksize;
                if count == windowsize || is_short {
                    let _ = client.send_to(&make_ack_packet(last_in_order), from).await;
                    count = 0;
                }
                if is_short {
                    break;
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {
                // No further block arrived: ACK the last in-order block to nudge
                // the server (covers the gap case where the window never filled
                // because a block was "lost").
                if last_in_order != 0 {
                    let _ = client
                        .send_to(&make_ack_packet(last_in_order), server_tid)
                        .await;
                    count = 0;
                }
            }
        }
    }
    received
}

#[tokio::test]
async fn test_tftp_windowsize_multi_window_transfer() {
    // A file spanning several windows transfers byte-exact when windowsize > 1.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    let blksize = 512usize;
    // 9 full blocks + a short 10th block => three windows of 4, 4, 2.
    let payload: Vec<u8> = (0..(9 * blksize + 137)).map(|i| (i % 256) as u8).collect();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), &payload).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25150", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25150", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = UdpSocket::bind("127.0.0.1:25151").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("windowsize", "4"), ("tsize", "0")],
    );
    client.send_to(&rrq, "127.0.0.1:25150").await.unwrap();

    let mut buf = [0u8; 1024];
    let (n, server_tid) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let oack = parse_oack(&buf[..n]);
    assert!(
        oack.contains(&("windowsize".to_string(), "4".to_string())),
        "OACK must echo the negotiated windowsize, got {oack:?}"
    );
    client
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();

    let got = windowed_recv_all(&client, server_tid, blksize, 4, None).await;
    assert_eq!(got.len(), payload.len(), "byte count must match");
    assert_eq!(got, payload, "windowed transfer must be byte-exact");
}

#[tokio::test]
async fn test_tftp_windowsize_recovers_lost_block() {
    // A block lost mid-window must be recovered via go-back-N so the file still
    // arrives byte-exact.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    let blksize = 512usize;
    // 5 full blocks + a short 6th block.
    let payload: Vec<u8> = (0..(5 * blksize + 100)).map(|i| (i % 251) as u8).collect();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), &payload).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25160", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_tftp::run_tftp_server("127.0.0.1:25160", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = UdpSocket::bind("127.0.0.1:25161").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("windowsize", "4"), ("tsize", "0")],
    );
    client.send_to(&rrq, "127.0.0.1:25160").await.unwrap();

    let mut buf = [0u8; 1024];
    let (n, server_tid) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let _ = parse_oack(&buf[..n]);
    client
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();

    // Drop block 3 once, forcing the server to roll back and resend from block 3.
    let got = windowed_recv_all(&client, server_tid, blksize, 4, Some(3)).await;
    assert_eq!(
        got.len(),
        payload.len(),
        "byte count must match after recovery"
    );
    assert_eq!(
        got, payload,
        "transfer must be byte-exact after a mid-window loss"
    );
}
