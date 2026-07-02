use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use dhcproto::{Decodable, Decoder, Encodable, Encoder, v4};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

#[tokio::test]
async fn test_dhcp_server_redirection() {
    // 1. Setup configuration
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24011".to_string(),
        cache_dir: "./cache".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
    };

    let host = HostConfig {
        mac: "00:11:22:33:44:55".to_string(),
        name: "test-host".to_string(),
        image_path: "/tmp/test.iso".into(),
        bootloader: Some("boot/special.efi".to_string()),
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    let config = Config {
        server: server_config,
        hosts: vec![host],
    };

    let shared_config = Arc::new(std::sync::RwLock::new(config));
    let state_store = StateStore::new();

    // 2. Spawn DHCP server
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24011", server_config_clone, server_store)
                .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Client socket to send request
    let client_socket = UdpSocket::bind("127.0.0.1:24012").await.unwrap();

    // Craft DHCP Inform packet
    let chaddr = vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
    let mut msg = v4::Message::default();
    msg.set_opcode(v4::Opcode::BootRequest)
        .set_chaddr(&chaddr)
        .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
        .opts_mut()
        .insert(v4::DhcpOption::MessageType(v4::MessageType::Inform));

    // Request UEFI x86-64 architecture
    msg.opts_mut()
        .insert(v4::DhcpOption::ClientSystemArchitecture(
            v4::Architecture::X64,
        ));

    let mut request_buf = Vec::new();
    let mut encoder = Encoder::new(&mut request_buf);
    msg.encode(&mut encoder).unwrap();

    // Send packet to DHCP server
    client_socket
        .send_to(&request_buf, "127.0.0.1:24011")
        .await
        .unwrap();

    // 4. Wait and receive response
    let mut response_buf = [0u8; 1500];
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let response = v4::Message::decode(&mut Decoder::new(&response_buf[..len])).unwrap();

    // 5. Assert options
    assert_eq!(response.opcode(), v4::Opcode::BootReply);

    // Verify MessageType is ACK
    let msg_type = response.opts().msg_type().unwrap();
    assert_eq!(msg_type, v4::MessageType::Ack);

    // Verify bootloader name is the host override
    let bootloader = response.opts().get(v4::OptionCode::BootfileName).unwrap();
    if let v4::DhcpOption::BootfileName(path_bytes) = bootloader {
        let path = String::from_utf8(path_bytes.clone()).unwrap();
        assert_eq!(path, "boot/special.efi");
    } else {
        panic!("Missing BootfileName option");
    }

    // Verify state store was updated
    let host_state = state_store.get_host("00:11:22:33:44:55").unwrap();
    assert_eq!(host_state.status, HostStatus::Polling);
    assert_eq!(host_state.name, Some("test-host".to_string()));
    assert_eq!(host_state.architecture, Some("x86_64".to_string()));
}
