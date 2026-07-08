use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use std::fs::{self, File};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::net::UdpSocket;

fn parse_oack(pkt: &[u8]) -> Vec<(String, String)> {
    assert!(pkt.len() >= 2);
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    assert_eq!(opcode, 6, "Expected OACK opcode");

    let mut parts = Vec::new();
    let mut current = Vec::new();
    for &b in &pkt[2..] {
        if b == 0 {
            parts.push(String::from_utf8(current).unwrap());
            current = Vec::new();
        } else {
            current.push(b);
        }
    }

    let mut options = Vec::new();
    let mut i = 0;
    while i + 1 < parts.len() {
        options.push((parts[i].clone(), parts[i + 1].clone()));
        i += 2;
    }
    options
}

fn parse_data(pkt: &[u8]) -> (u16, Vec<u8>) {
    assert!(pkt.len() >= 4);
    let opcode = u16::from_be_bytes([pkt[0], pkt[1]]);
    assert_eq!(opcode, 3, "Expected DATA opcode");
    let block = u16::from_be_bytes([pkt[2], pkt[3]]);
    (block, pkt[4..].to_vec())
}

fn make_rrq_packet(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&1u16.to_be_bytes()); // RRQ opcode
    pkt.extend_from_slice(filename.as_bytes());
    pkt.push(0);
    pkt.extend_from_slice(b"octet");
    pkt.push(0);
    for (k, v) in options {
        pkt.extend_from_slice(k.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(v.as_bytes());
        pkt.push(0);
    }
    pkt
}

fn make_ack_packet(block: u16) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&4u16.to_be_bytes()); // ACK opcode
    pkt.extend_from_slice(&block.to_be_bytes());
    pkt
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
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
        api_token: None,
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
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
        api_token: None,
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
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
        api_token: None,
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
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
        api_token: None,
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
