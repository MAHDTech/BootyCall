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
        oled_enabled: false,
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

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
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

/// Helper to create a minimal Config and StateStore, spawn the HTTP server on the
/// given port, and return the shared state objects for assertion.
async fn spawn_test_server(port: u16) -> (Arc<parking_lot::RwLock<Config>>, StateStore) {
    let tmp_dir = tempdir().unwrap();
    let cache_dir = tmp_dir.path().join("cache");
    fs::create_dir_all(&cache_dir).unwrap();

    let server_config = ServerConfig {
        http_bind: format!("127.0.0.1:{}", port),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: tmp_dir.path().to_path_buf(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir,
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
    };

    let host = HostConfig {
        mac: "aa:bb:cc:dd:ee:ff".to_string(),
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

    let shared_config = Arc::new(parking_lot::RwLock::new(config));
    let state_store = StateStore::new();

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    let bind_addr = format!("127.0.0.1:{}", port);

    // Leak the tempdir so it lives for the duration of the test
    let _leaked = Box::leak(Box::new(tmp_dir));

    tokio::spawn(async move {
        let _ =
            bootycall_http::run_http_server(&bind_addr, server_config_clone, server_store).await;
    });

    // Wait for server to bind
    tokio::time::sleep(Duration::from_millis(150)).await;

    (shared_config, state_store)
}

#[tokio::test]
async fn test_api_status_endpoint() {
    let port: u16 = 26081;
    let (_config, _state_store) = spawn_test_server(port).await;

    // GET /api/status should return 200 with JSON containing "hosts"
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                port
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();

    let (status, headers, body) = parse_http_response(&response);
    assert!(
        status.contains("200 OK"),
        "Expected 200 OK for /api/status, got: {}",
        status
    );

    // Verify content-type is JSON
    let content_type = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(
        content_type.contains("application/json"),
        "Expected JSON content-type, got: {}",
        content_type
    );

    // Verify body contains the "hosts" key
    let body_str = String::from_utf8(body).unwrap();
    assert!(
        body_str.contains("\"hosts\""),
        "Expected JSON body to contain 'hosts' key, got: {}",
        body_str
    );

    // Verify body also contains the "configs" key
    assert!(
        body_str.contains("\"configs\""),
        "Expected JSON body to contain 'configs' key, got: {}",
        body_str
    );

    // Verify the body is valid JSON and can be parsed
    let parsed: serde_json::Value =
        serde_json::from_str(&body_str).expect("Response body should be valid JSON");
    assert!(parsed["hosts"].is_array(), "hosts should be a JSON array");
    assert!(
        parsed["configs"].is_array(),
        "configs should be a JSON array"
    );
}

#[tokio::test]
async fn test_api_logs_endpoint() {
    let port: u16 = 26082;
    let (_config, state_store) = spawn_test_server(port).await;

    // Seed some log events so we can verify they appear
    state_store.log_event("INFO", Some("aa:bb:cc:dd:ee:ff"), "Test log entry");
    state_store.log_event("WARN", None, "Another test log");

    // GET /api/logs should return 200 with a JSON array
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET /api/logs HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                port
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();

    let (status, headers, body) = parse_http_response(&response);
    assert!(
        status.contains("200 OK"),
        "Expected 200 OK for /api/logs, got: {}",
        status
    );

    // Verify content-type is JSON
    let content_type = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert!(
        content_type.contains("application/json"),
        "Expected JSON content-type, got: {}",
        content_type
    );

    // Verify body is a valid JSON array
    let body_str = String::from_utf8(body).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&body_str).expect("Response body should be valid JSON");
    assert!(parsed.is_array(), "Expected JSON array body for /api/logs");

    // Verify our seeded log entries are present
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 2, "Expected 2 log entries, got {}", arr.len());
    assert_eq!(arr[0]["message"], "Test log entry");
    assert_eq!(arr[1]["message"], "Another test log");
}

async fn assert_http_traversal_blocked(port: u16, request_path: &str) {
    let (_config, _state_store) = spawn_test_server(port).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET {request_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();

    let (status, _headers, body) = parse_http_response(&response);
    // A rejected traversal must not disclose file contents. Accept 403 or 404
    // (the resolver returns None for absolute paths → 403; missing files → 404).
    assert!(
        status.contains("403") || status.contains("404"),
        "Expected 403/404 for {request_path:?}, got: {status}"
    );
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("root:"),
        "Response body must not contain /etc/passwd content for {request_path:?}"
    );
}

#[tokio::test]
async fn test_http_cache_absolute_path_blocked() {
    assert_http_traversal_blocked(26090, "/cache//etc/passwd").await;
}

#[tokio::test]
async fn test_http_cache_parent_dir_blocked() {
    assert_http_traversal_blocked(26091, "/cache/../../etc/passwd").await;
}

#[tokio::test]
async fn test_http_static_absolute_path_blocked() {
    assert_http_traversal_blocked(26092, "/static//etc/passwd").await;
}

#[tokio::test]
async fn test_http_static_parent_dir_blocked() {
    assert_http_traversal_blocked(26093, "/static/../../etc/passwd").await;
}

#[tokio::test]
async fn test_root_redirect() {
    let port: u16 = 26083;
    let (_config, _state_store) = spawn_test_server(port).await;

    // GET / should return 303 See Other with Location header pointing to /ui/index.html
    let mut client = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                port
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();

    let (status, headers, _body) = parse_http_response(&response);
    assert!(
        status.contains("303"),
        "Expected 303 See Other for root redirect, got: {}",
        status
    );

    // Verify location header points to /ui/index.html
    let location = headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    assert_eq!(
        location, "/ui/index.html",
        "Expected redirect to /ui/index.html, got: {}",
        location
    );
}
