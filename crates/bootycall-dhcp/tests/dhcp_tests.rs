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
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
        advertised_host: None,
        allowed_hosts: Vec::new(),
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

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
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

    // RFC 4578: advertise PXEClient in vendor-class identifier so the proxy
    // DHCP server will actually answer us.
    msg.opts_mut()
        .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));

    // Option 97: Client Machine Identifier (RFC 4578 / RFC 4578)
    let machine_id = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    msg.opts_mut()
        .insert(v4::DhcpOption::ClientMachineIdentifier(machine_id.clone()));

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

    // Verify Option 97 Client Machine Identifier is copied back
    let response_machine_id = response
        .opts()
        .get(v4::OptionCode::ClientMachineIdentifier)
        .unwrap();
    if let v4::DhcpOption::ClientMachineIdentifier(uuid) = response_machine_id {
        assert_eq!(*uuid, machine_id);
    } else {
        panic!("Missing or mismatching ClientMachineIdentifier option");
    }

    // Verify state store was updated
    let host_state = state_store.get_host("00:11:22:33:44:55").unwrap();
    assert_eq!(host_state.status, HostStatus::Polling);
    assert_eq!(host_state.name, Some("test-host".to_string()));
    assert_eq!(host_state.architecture, Some("x86_64".to_string()));
}

#[tokio::test]
async fn test_dhcp_server_ignores_non_pxe_client() {
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24021".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
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
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24021", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_socket = UdpSocket::bind("127.0.0.1:24022").await.unwrap();

    // Send a Discover WITHOUT DHCP Option 60. Under RFC 4578 the proxy DHCP
    // server must not reply to non-PXE clients, so we expect a timeout.
    let chaddr = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
    let mut msg = v4::Message::default();
    msg.set_opcode(v4::Opcode::BootRequest)
        .set_chaddr(&chaddr)
        .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
        .opts_mut()
        .insert(v4::DhcpOption::MessageType(v4::MessageType::Discover));
    // Note: no ClassIdentifier / no ClientSystemArchitecture.

    let mut request_buf = Vec::new();
    msg.encode(&mut Encoder::new(&mut request_buf)).unwrap();
    client_socket
        .send_to(&request_buf, "127.0.0.1:24021")
        .await
        .unwrap();

    let mut response_buf = [0u8; 1500];
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        client_socket.recv_from(&mut response_buf),
    )
    .await;
    assert!(
        result.is_err(),
        "Non-PXE Discover must not receive a reply, but the socket got one"
    );
}

#[tokio::test]
async fn test_dhcp_server_handles_arm64_arch() {
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24031".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
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
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24031", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_socket = UdpSocket::bind("127.0.0.1:24032").await.unwrap();

    let chaddr = vec![0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
    let mut msg = v4::Message::default();
    msg.set_opcode(v4::Opcode::BootRequest)
        .set_chaddr(&chaddr)
        .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
        .opts_mut()
        .insert(v4::DhcpOption::MessageType(v4::MessageType::Inform));
    // arch=11 → aarch64 EFI (RFC 4578 §2.1).
    msg.opts_mut()
        .insert(v4::DhcpOption::ClientSystemArchitecture(v4::Architecture(
            11,
        )));
    msg.opts_mut()
        .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));

    let mut request_buf = Vec::new();
    msg.encode(&mut Encoder::new(&mut request_buf)).unwrap();
    client_socket
        .send_to(&request_buf, "127.0.0.1:24031")
        .await
        .unwrap();

    let mut response_buf = [0u8; 1500];
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let response = v4::Message::decode(&mut Decoder::new(&response_buf[..len])).unwrap();
    let bootloader = response.opts().get(v4::OptionCode::BootfileName).unwrap();
    if let v4::DhcpOption::BootfileName(path_bytes) = bootloader {
        let path = String::from_utf8(path_bytes.clone()).unwrap();
        assert_eq!(
            path, "boot/arm64/ipxe.efi",
            "arch=11 should serve the arm64 default bootloader"
        );
    } else {
        panic!("Missing BootfileName option");
    }

    let host_state = state_store.get_host("00:aa:bb:cc:dd:ee").unwrap();
    assert_eq!(host_state.architecture, Some("aarch64".to_string()));
}

#[tokio::test]
async fn test_dhcp_arch0_serves_bios_bootloader() {
    // A legacy BIOS PXE client (Option 93 architecture 0) must get the BIOS
    // bootloader, not the amd64 EFI default it cannot execute (issue 033).
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24041".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/bios/undionly.kpxe".to_string(),
        oled_enabled: false,
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
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24041", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_socket = UdpSocket::bind("127.0.0.1:24042").await.unwrap();

    let chaddr = vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    let mut msg = v4::Message::default();
    msg.set_opcode(v4::Opcode::BootRequest)
        .set_chaddr(&chaddr)
        .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
        .opts_mut()
        .insert(v4::DhcpOption::MessageType(v4::MessageType::Inform));
    // arch=0 → legacy BIOS PXE (RFC 5970 §3.3).
    msg.opts_mut()
        .insert(v4::DhcpOption::ClientSystemArchitecture(v4::Architecture(
            0,
        )));
    msg.opts_mut()
        .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));

    let mut request_buf = Vec::new();
    msg.encode(&mut Encoder::new(&mut request_buf)).unwrap();
    client_socket
        .send_to(&request_buf, "127.0.0.1:24041")
        .await
        .unwrap();

    let mut response_buf = [0u8; 1500];
    let (len, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_socket.recv_from(&mut response_buf),
    )
    .await
    .unwrap()
    .unwrap();

    let response = v4::Message::decode(&mut Decoder::new(&response_buf[..len])).unwrap();
    let bootloader = response.opts().get(v4::OptionCode::BootfileName).unwrap();
    if let v4::DhcpOption::BootfileName(path_bytes) = bootloader {
        let path = String::from_utf8(path_bytes.clone()).unwrap();
        assert_eq!(
            path, "boot/bios/undionly.kpxe",
            "arch=0 should serve the BIOS bootloader, not the amd64 EFI default"
        );
    } else {
        panic!("Missing BootfileName option");
    }

    let host_state = state_store.get_host("02:00:00:00:00:01").unwrap();
    assert_eq!(host_state.architecture, Some("x86 (BIOS)".to_string()));
}

#[tokio::test]
async fn test_dhcp_unsupported_arch_gets_no_response() {
    // A client advertising an architecture we have no image for (IA32 EFI,
    // arch 6, or ARM32 EFI, arch 10) must get *no* offer: silently serving
    // the amd64 EFI default would hand it a binary it cannot execute.
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24061".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
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
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24061", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_socket = UdpSocket::bind("127.0.0.1:24062").await.unwrap();

    for (arch_code, chaddr_tail) in [(6u16, 0x06u8), (10, 0x0a)] {
        // A fully valid PXE Inform (Option 60 = PXEClient) so the *only*
        // reason for silence is the unsupported architecture code.
        let chaddr = vec![0x02, 0x00, 0x00, 0x00, 0x00, chaddr_tail];
        let mut msg = v4::Message::default();
        msg.set_opcode(v4::Opcode::BootRequest)
            .set_chaddr(&chaddr)
            .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(v4::MessageType::Inform));
        msg.opts_mut()
            .insert(v4::DhcpOption::ClientSystemArchitecture(v4::Architecture(
                arch_code,
            )));
        msg.opts_mut()
            .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));

        let mut request_buf = Vec::new();
        msg.encode(&mut Encoder::new(&mut request_buf)).unwrap();
        client_socket
            .send_to(&request_buf, "127.0.0.1:24061")
            .await
            .unwrap();

        let mut response_buf = [0u8; 1500];
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            client_socket.recv_from(&mut response_buf),
        )
        .await;
        assert!(
            result.is_err(),
            "arch {arch_code} must not receive a reply, but the socket got one"
        );
    }
}

#[tokio::test]
async fn test_dhcp_ignores_release_message() {
    // Message-type filtering: only Discover/Request/Inform are answered. A
    // Release (even from a valid PXEClient) must yield no reply.
    let server_config = ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: "./tftpboot".into(),
        proxy_dhcp_bind: "127.0.0.1:24051".to_string(),
        cache_dir: "./cache".into(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
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
        let _ =
            bootycall_dhcp::run_dhcp_server("127.0.0.1:24051", server_config_clone, server_store)
                .await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_socket = UdpSocket::bind("127.0.0.1:24052").await.unwrap();

    let chaddr = vec![0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
    let mut msg = v4::Message::default();
    msg.set_opcode(v4::Opcode::BootRequest)
        .set_chaddr(&chaddr)
        .set_ciaddr(Ipv4Addr::new(127, 0, 0, 1))
        .opts_mut()
        .insert(v4::DhcpOption::MessageType(v4::MessageType::Release));
    // A valid PXEClient + arch so the *only* reason for silence is the
    // message type, not the RFC 4578 vendor-class filter.
    msg.opts_mut()
        .insert(v4::DhcpOption::ClientSystemArchitecture(
            v4::Architecture::X64,
        ));
    msg.opts_mut()
        .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));

    let mut request_buf = Vec::new();
    msg.encode(&mut Encoder::new(&mut request_buf)).unwrap();
    client_socket
        .send_to(&request_buf, "127.0.0.1:24051")
        .await
        .unwrap();

    let mut response_buf = [0u8; 1500];
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        client_socket.recv_from(&mut response_buf),
    )
    .await;
    assert!(
        result.is_err(),
        "a DHCP Release must not receive a reply, but the socket got one"
    );
}
