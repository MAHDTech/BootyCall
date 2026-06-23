use bootycall_core::config::{Config, HostConfig, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use std::fs::{self, File};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn parse_http_response(bytes: &[u8]) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut split_idx = 0;
    for i in 0..bytes.len() - 3 {
        if &bytes[i..i + 4] == b"\r\n\r\n" {
            split_idx = i;
            break;
        }
    }

    let header_part = String::from_utf8(bytes[..split_idx].to_vec()).unwrap();
    let body = bytes[split_idx + 4..].to_vec();

    let mut lines = header_part.lines();
    let status_line = lines.next().unwrap().to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }

    (status_line, headers, body)
}

#[tokio::test]
async fn test_http_server_endpoints() {
    // 1. Setup temporary directory for cache and wallpapers
    let tmp_dir = tempdir().unwrap();
    let cache_dir = tmp_dir.path().join("cache");
    let wallpapers_dir = tmp_dir.path().join("static/wallpapers");

    fs::create_dir_all(&cache_dir).unwrap();
    fs::create_dir_all(&wallpapers_dir).unwrap();

    // Create a mock wallpaper image
    let wp_file = wallpapers_dir.join("bg.png");
    {
        let mut file = File::create(&wp_file).unwrap();
        file.write_all(b"PNGIMAGE").unwrap();
    }

    // Create a mock cache file for nixos MAC
    let host_mac = "aa:bb:cc:dd:ee:ff";
    let host_cache_dir = cache_dir.join(host_mac);
    fs::create_dir_all(&host_cache_dir).unwrap();
    {
        let mut f_kern = File::create(host_cache_dir.join("kernel")).unwrap();
        f_kern.write_all(b"KERNELBYTES").unwrap();
        let mut f_init = File::create(host_cache_dir.join("initrd")).unwrap();
        f_init.write_all(b"INITRDBYTES").unwrap();
    }

    // 2. Setup server configuration
    let server_config = ServerConfig {
        http_bind: "127.0.0.1:26080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: tmp_dir.path().to_path_buf(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: cache_dir.clone(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
    };

    let host = HostConfig {
        mac: host_mac.to_string(),
        name: "nixos-test".to_string(),
        image_path: "/tmp/nixos.iso".into(),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: Some("console=tty0".to_string()),
    };

    let config = Config {
        server: server_config,
        hosts: vec![host],
    };

    let shared_config = Arc::new(std::sync::RwLock::new(config));
    let state_store = StateStore::new();

    // 3. Spawn HTTP Server
    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ =
            bootycall_http::run_http_server("127.0.0.1:26080", server_config_clone, server_store)
                .await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(150)).await;

    // 4. Test GET /start endpoint
    {
        let mut client = TcpStream::connect("127.0.0.1:26080").await.unwrap();
        client
            .write_all(b"GET /start HTTP/1.1\r\nHost: 127.0.0.1:26080\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        let (status, _, body) = parse_http_response(&response);
        assert!(status.contains("200 OK"));
        let body_str = String::from_utf8(body).unwrap();
        assert!(
            body_str
                .contains("chain --autofree --replace http://127.0.0.1:26080/poll/${mac:hexhyp}")
        );
    }

    // 5. Test GET /poll/aa-bb-cc-dd-ee-ff (Mapped Host)
    {
        let mut client = TcpStream::connect("127.0.0.1:26080").await.unwrap();
        // Send request from standard client local IP mapping
        client.write_all(b"GET /poll/aa-bb-cc-dd-ee-ff HTTP/1.1\r\nHost: 127.0.0.1:26080\r\nConnection: close\r\n\r\n").await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        let (status, _, body) = parse_http_response(&response);
        assert!(status.contains("200 OK"));
        let body_str = String::from_utf8(body).unwrap();
        // Should return actual boot execution script since host is in yaml config
        assert!(
            body_str.contains(
                "kernel http://127.0.0.1:26080/cache/aa:bb:cc:dd:ee:ff/kernel console=tty0"
            )
        );
        assert!(body_str.contains("initrd http://127.0.0.1:26080/cache/aa:bb:cc:dd:ee:ff/initrd"));
        assert!(body_str.contains("boot"));

        // StateStore should show HostStatus::Booting
        let host_state = state_store.get_host(host_mac).unwrap();
        assert_eq!(host_state.status, HostStatus::Booting);
    }

    // 6. Test GET /poll/11-22-33-44-55-66 (Unmapped Host)
    {
        let mut client = TcpStream::connect("127.0.0.1:26080").await.unwrap();
        client.write_all(b"GET /poll/11-22-33-44-55-66 HTTP/1.1\r\nHost: 127.0.0.1:26080\r\nConnection: close\r\n\r\n").await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        let (status, _, body) = parse_http_response(&response);
        assert!(status.contains("200 OK"));
        let body_str = String::from_utf8(body).unwrap();
        // Should return the retry script prompting Ctrl-B override
        assert!(body_str.contains("BootyCall: Press Ctrl-B for manual override..."));
        assert!(body_str.contains("poll/11:22:33:44:55:66"));

        // StateStore should register unmapped host as Polling
        let unmapped_state = state_store.get_host("11:22:33:44:55:66").unwrap();
        assert_eq!(unmapped_state.status, HostStatus::Polling);
    }

    // 7. Test API override: POST /api/override assigning nixos-test to 11:22:33:44:55:66
    {
        let mut client = TcpStream::connect("127.0.0.1:26080").await.unwrap();
        let payload = r#"{"mac":"11:22:33:44:55:66","target":"nixos-test"}"#;
        let req = format!(
            "POST /api/override HTTP/1.1\r\nHost: 127.0.0.1:26080\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            payload.len(),
            payload
        );
        client.write_all(req.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        let (status, _, body) = parse_http_response(&response);
        assert!(status.contains("200 OK"));
        let body_str = String::from_utf8(body).unwrap();
        assert!(body_str.contains("\"status\":\"ok\""));

        // Host in StateStore should have assigned target set
        let state = state_store.get_host("11:22:33:44:55:66").unwrap();
        assert_eq!(state.assigned_target, Some("nixos-test".to_string()));
    }

    // 8. Re-test GET /poll/11-22-33-44-55-66 (Now overridden)
    {
        let mut client = TcpStream::connect("127.0.0.1:26080").await.unwrap();
        client.write_all(b"GET /poll/11-22-33-44-55-66 HTTP/1.1\r\nHost: 127.0.0.1:26080\r\nConnection: close\r\n\r\n").await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        let (status, _, body) = parse_http_response(&response);
        assert!(status.contains("200 OK"));
        let body_str = String::from_utf8(body).unwrap();
        // Should now return the boot execution script pointing to the cache files of the nixos-test (MAC: aa:bb:cc:dd:ee:ff)
        assert!(
            body_str.contains(
                "kernel http://127.0.0.1:26080/cache/aa:bb:cc:dd:ee:ff/kernel console=tty0"
            )
        );
        assert!(body_str.contains("initrd http://127.0.0.1:26080/cache/aa:bb:cc:dd:ee:ff/initrd"));

        // Status should change to Booting
        let state = state_store.get_host("11:22:33:44:55:66").unwrap();
        assert_eq!(state.status, HostStatus::Booting);
    }
}
