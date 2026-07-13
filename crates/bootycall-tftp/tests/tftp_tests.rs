use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use std::fs::{self, File};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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
    state_store
        .update_host_status(
            "00:aa:bb:cc:dd:ee",
            HostStatus::Polling,
            Some("test-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    // 3. Spawn TFTP Server
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25069",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    let (code, msg) =
        bootycall_tftp::wire::parse_error_packet(pkt).expect("expected an ERROR packet");
    (opcode, code, msg)
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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25074",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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
        let _ = bootycall_tftp::run_tftp_server(
            &bind,
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25106",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
    };
    let config = Config {
        server: server_config,
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map 127.0.0.1 -> a known MAC so the transfer associates a host.
    let mac = "00:11:22:33:44:55";
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("failing-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25120",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
    for _ in 0..140 {
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
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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
            CancellationToken::new(),
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
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("busy-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server_with_limit(
            "127.0.0.1:25170",
            server_config_clone,
            server_store,
            1,
            CancellationToken::new(),
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
async fn test_tftp_server_ipv6_negotiation_and_transfer() {
    // An IPv6 client against a `[::]`-bound listener must complete a full
    // transfer. The per-transfer socket used to be hard-bound to IPv4
    // `0.0.0.0:0`, so `connect(src_addr)` failed on the address-family
    // mismatch and the client never even received the OACK — it just timed
    // out. The transfer socket must instead bind `[::]:0` for IPv6 peers.
    //
    // Probe for IPv6 support first: some sandboxes/CI runners have no IPv6
    // stack at all, in which case this test skips instead of failing.
    if UdpSocket::bind("[::1]:0").await.is_err() {
        eprintln!(
            "skipping test_tftp_server_ipv6_negotiation_and_transfer: IPv6 unavailable in this environment"
        );
        return;
    }

    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    let file_content = b"This is the IPv6-served EFI payload!";
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), file_content).unwrap();

    let config = Config {
        server: base_server_config("[::]:25180", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map ::1 -> a known MAC so the transfer exercises the SocketAddr::V6
    // branch of the client-IP host lookup and the status transitions.
    let mac = "00:aa:bb:cc:dd:60";
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("ipv6-client".to_string()),
            None,
            Some("::1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "[::]:25180",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // IPv6 client over loopback.
    let client_socket = UdpSocket::bind("[::1]:25181").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );
    client_socket.send_to(&rrq, "[::1]:25180").await.unwrap();

    // Receive the OACK — before the fix this timed out because the IPv4
    // transfer socket could not connect to the IPv6 peer.
    let mut response_buf = [0u8; 1024];
    let (len, server_tid_addr) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("IPv6 client should receive an OACK from the transfer socket")
    .unwrap();
    assert!(
        server_tid_addr.is_ipv6(),
        "transfer socket must answer from an IPv6 address, got {server_tid_addr}"
    );

    let negotiated_options = parse_oack(&response_buf[..len]);
    assert!(negotiated_options.contains(&("blksize".to_string(), "512".to_string())));
    assert!(negotiated_options.contains(&("timeout".to_string(), "1".to_string())));
    assert!(negotiated_options.contains(&("tsize".to_string(), file_content.len().to_string())));

    // The V6 client IP must have mapped to the known host.
    assert_eq!(
        state_store.get_host(mac).map(|h| h.status),
        Some(HostStatus::Booting),
        "host mapped by its IPv6 client IP should be Booting"
    );

    // ACK block 0, receive DATA block 1, ACK it.
    client_socket
        .send_to(&make_ack_packet(0), server_tid_addr)
        .await
        .unwrap();
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("IPv6 client should receive DATA block 1")
    .unwrap();
    let (block_num, data) = parse_data(&response_buf[..len]);
    assert_eq!(block_num, 1);
    assert_eq!(data, file_content.to_vec());
    client_socket
        .send_to(&make_ack_packet(1), server_tid_addr)
        .await
        .unwrap();

    // The completion update is asynchronous; poll until the host completes.
    let mut completed = false;
    for _ in 0..40 {
        if let Some(hs) = state_store.get_host(mac)
            && hs.status == HostStatus::Completed
        {
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        completed,
        "host should be Completed after the IPv6 transfer, got {:?}",
        state_store.get_host(mac).map(|h| h.status)
    );
}

#[tokio::test]
async fn test_tftp_bios_default_redirected_to_override() {
    // A legacy-BIOS host that requests the *BIOS* default NBP must be
    // redirected to its per-MAC bootloader override, exactly like the
    // amd64/arm64 EFI defaults already are (the redirect check used to omit
    // `default_bootloader_bios`).
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    // The BIOS default exists with decoy content: if the redirect regresses,
    // the client receives these bytes and the assertions below catch it.
    let decoy_content = b"decoy legacy BIOS NBP payload";
    fs::write(tftp_root.join("boot/x64/undionly.kpxe"), decoy_content).unwrap();
    let special_content = b"override payload served instead of the BIOS default";
    fs::write(tftp_root.join("boot/special.efi"), special_content).unwrap();

    let host = HostConfig {
        mac: "00:aa:bb:cc:dd:72".to_string(),
        name: "bios-client".to_string(),
        image_path: "/tmp/nixos.iso".into(),
        bootloader: Some("boot/special.efi".to_string()),
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    let config = Config {
        server: base_server_config("127.0.0.1:25190", &tftp_root),
        hosts: vec![host],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map 127.0.0.1 -> the overridden MAC so the RRQ associates the host.
    let mac = "00:aa:bb:cc:dd:72";
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("bios-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86 (BIOS)".to_string()),
        )
        .unwrap();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25190",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The client asks for the BIOS default (`default_bootloader_bios`).
    let client_socket = UdpSocket::bind("127.0.0.1:25191").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/undionly.kpxe",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );
    client_socket
        .send_to(&rrq, "127.0.0.1:25190")
        .await
        .unwrap();

    // The OACK's tsize must already reflect the override file, not the decoy.
    let mut response_buf = [0u8; 1024];
    let (len, server_tid_addr) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("BIOS client should receive an OACK")
    .unwrap();
    let negotiated_options = parse_oack(&response_buf[..len]);
    assert!(
        negotiated_options.contains(&("tsize".to_string(), special_content.len().to_string())),
        "OACK tsize must be the override file's size, got {negotiated_options:?}"
    );

    // ACK block 0, then the DATA must carry the override bytes.
    client_socket
        .send_to(&make_ack_packet(0), server_tid_addr)
        .await
        .unwrap();
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .expect("BIOS client should receive DATA block 1")
    .unwrap();
    let (block_num, data) = parse_data(&response_buf[..len]);
    assert_eq!(block_num, 1);
    assert_eq!(
        data,
        special_content.to_vec(),
        "BIOS-default request must be redirected to the per-MAC override"
    );
    assert_ne!(
        data,
        decoy_content.to_vec(),
        "the decoy BIOS default must not be served"
    );
    client_socket
        .send_to(&make_ack_packet(1), server_tid_addr)
        .await
        .unwrap();
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
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25140",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25150",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25160",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
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

#[tokio::test]
async fn test_tftp_non_utf8_option_ignored_transfer_proceeds() {
    // RFC 2347: a server ignores options it cannot parse. An RRQ carrying one
    // option pair with non-UTF-8 bytes must NOT abort the request — the
    // transfer proceeds, and options after the bad pair still negotiate
    // (previously the `?` on `from_utf8` failed the whole parse and the
    // request was silently dropped, so the client just timed out).
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    let file_content = b"payload served despite a non-UTF-8 option";
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), file_content).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25200", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25200",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Hand-build the RRQ: opcode 1, filename\0, octet\0, then a non-UTF-8
    // option pair (0xFF 0xFE key), followed by a valid `tsize` option that
    // must still be honoured after the bad pair is skipped.
    let mut rrq = vec![0x00, 0x01];
    rrq.extend_from_slice(b"boot/x64/ipxe.efi");
    rrq.push(0);
    rrq.extend_from_slice(b"octet");
    rrq.push(0);
    rrq.extend_from_slice(&[0xFF, 0xFE]); // non-UTF-8 option key
    rrq.push(0);
    rrq.extend_from_slice(b"junk");
    rrq.push(0);
    rrq.extend_from_slice(b"tsize");
    rrq.push(0);
    rrq.extend_from_slice(b"0");
    rrq.push(0);

    let client = UdpSocket::bind("127.0.0.1:25201").await.unwrap();
    client.send_to(&rrq, "127.0.0.1:25200").await.unwrap();

    // The `tsize` option after the skipped pair still negotiates: an OACK
    // arrives instead of a silent drop (the pre-fix behaviour).
    let mut buf = [0u8; 1024];
    let (n, server_tid) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("RRQ with a non-UTF-8 option must still be served, not dropped")
        .unwrap();
    let oack = parse_oack(&buf[..n]);
    assert!(
        oack.contains(&("tsize".to_string(), file_content.len().to_string())),
        "the valid tsize option after the bad pair must still negotiate, got {oack:?}"
    );

    // Complete the transfer: ACK 0, receive DATA block 1, verify the bytes.
    client
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("DATA block 1 should arrive")
        .unwrap();
    let (block, data) = parse_data(&buf[..n]);
    assert_eq!(block, 1);
    assert_eq!(data, file_content.to_vec());
    client
        .send_to(&make_ack_packet(1), server_tid)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_tftp_malformed_rrq_draws_error_code_4() {
    // A packet with a valid RRQ opcode but an unparseable body (no
    // null-terminated filename/mode) must draw ERROR code 4 (Illegal TFTP
    // operation) so the client fails fast, instead of the pre-fix silent
    // drop that left it timing out.
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    let config = Config {
        server: base_server_config("127.0.0.1:25210", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25210",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // RRQ opcode followed by garbage with no null terminators: parse_rrq
    // cannot extract a filename/mode pair.
    let malformed = [0x00u8, 0x01, 0xFF, 0xFE, 0xFD];

    let client = UdpSocket::bind("127.0.0.1:25211").await.unwrap();
    client.send_to(&malformed, "127.0.0.1:25210").await.unwrap();

    let mut buf = [0u8; 1024];
    let (n, _from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("malformed RRQ should draw an ERROR reply, not a silent drop")
        .unwrap();
    let (opcode, code, msg) = parse_error_packet(&buf[..n]);
    assert_eq!(opcode, 5, "expected a TFTP ERROR packet (opcode 5)");
    assert_eq!(code, 4, "malformed RRQ uses illegal-operation code 4");
    assert!(
        msg.contains("Illegal TFTP operation"),
        "message should mention the illegal operation, got {msg:?}"
    );
}

#[tokio::test]
async fn test_tftp_blksize_negotiation_clamps_downward() {
    // 1. Setup temporary directory for tftp root and a test file
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    let file_content = b"This is the default EFI payload!";
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), file_content).unwrap();

    // 2. Setup server configuration
    let config = Config {
        server: base_server_config("127.0.0.1:25220", &tftp_root),
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // 3. Spawn TFTP Server
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25220",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Client socket to send request with blksize=128
    let client_socket = UdpSocket::bind("127.0.0.1:25221").await.unwrap();

    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "128"), ("timeout", "1"), ("tsize", "0")],
    );

    client_socket
        .send_to(&rrq, "127.0.0.1:25220")
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

    // Assert that the negotiated blksize is 128 (not 512, which is DEFAULT_BLKSIZE)
    assert!(negotiated_options.contains(&("blksize".to_string(), "128".to_string())));

    // Complete the transfer to verify it works with the custom block size
    client_socket
        .send_to(&make_ack_packet(0), server_tid_addr)
        .await
        .unwrap();

    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let (block_num, data) = parse_data(&response_buf[..len]);
    assert_eq!(block_num, 1);
    assert_eq!(data, file_content.to_vec());

    client_socket
        .send_to(&make_ack_packet(1), server_tid_addr)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_tftp_blksize_negotiation_defaults_to_512() {
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    let file_content = b"This is the default EFI payload!";
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), file_content).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25230", &tftp_root),
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25230",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // No blksize option requested, only timeout & tsize
    let client_socket = UdpSocket::bind("127.0.0.1:25231").await.unwrap();
    let rrq = make_rrq_packet("boot/x64/ipxe.efi", &[("timeout", "1"), ("tsize", "0")]);

    client_socket
        .send_to(&rrq, "127.0.0.1:25230")
        .await
        .unwrap();

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
    assert!(!negotiated_options.iter().any(|(k, _)| k == "blksize"));

    client_socket
        .send_to(&make_ack_packet(0), server_tid_addr)
        .await
        .unwrap();

    // The block size should default to 512, which means the DATA block will contain the whole payload.
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let (block_num, data) = parse_data(&response_buf[..len]);
    assert_eq!(block_num, 1);
    assert_eq!(data, file_content.to_vec());
}

#[tokio::test]
async fn test_tftp_windowsize_5_packet_loss_recovery() {
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    // Create a file of 10 blocks (10 * 512 = 5120 bytes)
    let block_size = 512;
    let mut file_content = vec![0u8; 10 * block_size];
    for i in 0..file_content.len() {
        file_content[i] = (i % 256) as u8;
    }
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), &file_content).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25240", &tftp_root),
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25240",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client requests windowsize = 7
    let client_socket = UdpSocket::bind("127.0.0.1:25241").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("windowsize", "7")],
    );

    client_socket
        .send_to(&rrq, "127.0.0.1:25240")
        .await
        .unwrap();

    let mut response_buf = [0u8; 1024];
    let (len, server_tid_addr) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let negotiated_options = parse_oack(&response_buf[..len]);
    assert!(negotiated_options.contains(&("windowsize".to_string(), "7".to_string())));

    // Send ACK for block 0 to start transfer
    client_socket
        .send_to(&make_ack_packet(0), server_tid_addr)
        .await
        .unwrap();

    let mut next_expected = 1;
    let mut received_bytes = Vec::new();
    let mut num_dup_ack_sent = 0;

    loop {
        let (len, _) = match tokio::time::timeout(
            Duration::from_secs(3),
            client_socket.recv_from(&mut response_buf),
        )
        .await
        {
            Ok(Ok(res)) => res,
            _ => {
                break;
            }
        };

        let (block_num, data) = parse_data(&response_buf[..len]);

        if block_num == next_expected {
            if block_num == 2 && num_dup_ack_sent == 0 {
                // Drop block 2. Do not advance next_expected, and do not ACK.
                continue;
            }

            received_bytes.extend_from_slice(&data);
            next_expected += 1;

            client_socket
                .send_to(&make_ack_packet(block_num), server_tid_addr)
                .await
                .unwrap();

            if block_num == 10 {
                break;
            }
        } else if block_num > next_expected {
            let dup_ack_val = next_expected - 1;
            client_socket
                .send_to(&make_ack_packet(dup_ack_val), server_tid_addr)
                .await
                .unwrap();
            num_dup_ack_sent += 1;
        } else {
            let dup_ack_val = next_expected - 1;
            client_socket
                .send_to(&make_ack_packet(dup_ack_val), server_tid_addr)
                .await
                .unwrap();
        }
    }

    assert_eq!(received_bytes, file_content);
    assert!(
        num_dup_ack_sent >= 5,
        "should have sent at least 5 duplicate ACKs to test abort protection, sent {}",
        num_dup_ack_sent
    );
}

#[tokio::test]
async fn test_tftp_windowsize_no_redundant_transmissions_loss_free() {
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();

    let blksize = 512usize;
    // A file spanning 8 blocks
    let payload: Vec<u8> = (0..(8 * blksize)).map(|i| (i % 256) as u8).collect();
    fs::write(tftp_root.join("boot/x64/ipxe.efi"), &payload).unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25260", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25260",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = UdpSocket::bind("127.0.0.1:25261").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("windowsize", "4"), ("tsize", "0")],
    );
    client.send_to(&rrq, "127.0.0.1:25260").await.unwrap();

    let mut buf = [0u8; 1024];
    let (n, server_tid) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let _oack = parse_oack(&buf[..n]);
    client
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();

    let mut received_blocks = Vec::new();
    let mut received_bytes = Vec::new();

    loop {
        let (n, _) =
            match tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf)).await {
                Ok(Ok(res)) => res,
                _ => break,
            };
        let (block, data) = parse_data(&buf[..n]);
        received_blocks.push(block);
        received_bytes.extend_from_slice(&data);

        // Send ACK for this block immediately to trigger intermediate ACKs
        client
            .send_to(&make_ack_packet(block), server_tid)
            .await
            .unwrap();

        if data.len() < blksize {
            break;
        }
    }

    assert_eq!(received_bytes, payload);
    // Under loss-free conditions, each block must be transmitted exactly once.
    // Since the file is exactly 8 blocks, a 9th empty block (block 9) is sent to signal EOF.
    let expected_blocks: Vec<u16> = (1..=9).collect();
    assert_eq!(
        received_blocks, expected_blocks,
        "Each block must be sent exactly once under loss-free conditions"
    );
}

#[tokio::test]
async fn test_tftp_garbage_flood_aborts_transfer() {
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();
    fs::create_dir_all(tftp_root.join("boot/x64")).unwrap();
    fs::write(
        tftp_root.join("boot/x64/ipxe.efi"),
        b"payload-for-garbage-flood-test",
    )
    .unwrap();

    let config = Config {
        server: base_server_config("127.0.0.1:25290", &tftp_root),
        hosts: vec![],
    };
    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let mac = "00:aa:bb:cc:dd:99";
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("garbage-flood-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25290",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client_socket = UdpSocket::bind("127.0.0.1:25291").await.unwrap();
    let rrq = make_rrq_packet(
        "boot/x64/ipxe.efi",
        &[("blksize", "512"), ("timeout", "1"), ("tsize", "0")],
    );
    client_socket
        .send_to(&rrq, "127.0.0.1:25290")
        .await
        .unwrap();

    // Consume OACK
    let mut buf = [0u8; 1024];
    let (n, server_tid) =
        tokio::time::timeout(Duration::from_secs(2), client_socket.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
    let _ = parse_oack(&buf[..n]);

    // Send ACK 0
    client_socket
        .send_to(&make_ack_packet(0), server_tid)
        .await
        .unwrap();

    // Receive DATA block 1
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), client_socket.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let (block_num, _) = parse_data(&buf[..n]);
    assert_eq!(block_num, 1);

    // Flood garbage packets/incorrect ACKs to server_tid to trigger early abort.
    for _ in 0..10 {
        // Send random garbage bytes
        let _ = client_socket
            .send_to(b"GARBAGE_PACKET_DATA_FLOOD", server_tid)
            .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Verify host state transitions to Failed
    let mut failed = false;
    for _ in 0..100 {
        if let Some(hs) = state_store.get_host(mac)
            && hs.status == HostStatus::Failed
        {
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        failed,
        "host should be marked Failed after garbage flood aborts transfer, got {:?}",
        state_store.get_host(mac).map(|h| h.status)
    );
}

#[tokio::test]
async fn test_tftp_directory_rejected() {
    // Setup temporary directory for tftp root
    let tmp_dir = tempdir().unwrap();
    let tftp_root = tmp_dir.path().to_path_buf();

    // Create a directory inside the tftp root
    let dir_path = tftp_root.join("boot/x64/some_directory");
    fs::create_dir_all(&dir_path).unwrap();

    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "127.0.0.1:25300".to_string(),
        tftp_root: tftp_root.clone(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        led_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
    };

    let config = Config {
        server: server_config,
        hosts: vec![],
    };

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    // Map 127.0.0.1 -> a known MAC so the transfer associates a host.
    let mac = "00:aa:bb:cc:dd:99";
    state_store
        .update_host_status(
            mac,
            HostStatus::Polling,
            Some("dir-client".to_string()),
            None,
            Some("127.0.0.1".to_string()),
            Some("x86_64".to_string()),
        )
        .unwrap();

    // Spawn TFTP server on port 25300
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_tftp::run_tftp_server(
            "127.0.0.1:25300",
            server_config_clone,
            server_store,
            CancellationToken::new(),
        )
        .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client socket on a unique port
    let client_socket = UdpSocket::bind("127.0.0.1:25301").await.unwrap();

    // Request the directory instead of a regular file
    let rrq = make_rrq_packet("boot/x64/some_directory", &[]);
    client_socket
        .send_to(&rrq, "127.0.0.1:25300")
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

    // Verify state store updated to Failed (state leak prevention)
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
        "host should be marked Failed after the directory transfer reject, got {:?}",
        state_store.get_host(mac).map(|h| h.status)
    );
}
