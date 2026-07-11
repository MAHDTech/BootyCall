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

#[tokio::test]
async fn test_poll_rejects_invalid_mac() {
    let port: u16 = 26100;
    let (_config, _state_store) = spawn_test_server(port).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET /poll/zz-bb-cc-dd-ee-ff HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, _) = parse_http_response(&response);
    assert!(
        status.contains("400"),
        "Invalid MAC must return 400, got: {status}"
    );
}

#[tokio::test]
async fn test_override_rejects_invalid_mac() {
    let port: u16 = 26101;
    let (_config, _state_store) = spawn_test_server(port).await;

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let body = r#"{"mac":"zz:bb:cc:dd:ee:ff","target":"nixos-test"}"#;
    let req = format!(
        "POST /api/override HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, _) = parse_http_response(&response);
    assert!(
        status.contains("400"),
        "Invalid MAC on override must return 400, got: {status}"
    );
}

#[tokio::test]
async fn test_override_requires_api_token_when_configured() {
    let port: u16 = 26102;
    let (config, _state_store) = spawn_test_server(port).await;
    {
        let mut guard = config.write();
        guard.server.api_token = Some("super-secret".to_string());
    }

    // No token → 401
    {
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let body = r#"{"mac":"aa:bb:cc:dd:ee:ff","target":"nixos-test"}"#;
        let req = format!(
            "POST /api/override HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        client.write_all(req.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let (status, _, _) = parse_http_response(&response);
        assert!(
            status.contains("401"),
            "Missing X-API-Token must return 401, got: {status}"
        );
    }

    // Correct token → 200
    {
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let body = r#"{"mac":"aa:bb:cc:dd:ee:ff","target":"nixos-test"}"#;
        let req = format!(
            "POST /api/override HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nX-API-Token: super-secret\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        client.write_all(req.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let (status, _, _) = parse_http_response(&response);
        assert!(
            status.contains("200"),
            "Correct token must succeed, got: {status}"
        );
    }
}

#[tokio::test]
async fn test_poll_missing_override_target_keeps_polling() {
    // A host registered with an override target that no longer exists in config
    // must keep polling (200 + retry script), not boot (issue 010/046).
    let port: u16 = 26103;
    let (_config, state_store) = spawn_test_server(port).await;
    state_store.update_host_status(
        "11:22:33:44:55:66",
        HostStatus::Polling,
        None,
        Some("does-not-exist".to_string()),
        None,
        None,
    );

    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET /poll/11-22-33-44-55-66 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, body) = parse_http_response(&response);
    let body_str = String::from_utf8_lossy(&body);

    assert!(
        status.contains("200"),
        "missing target must still 200, got: {status}"
    );
    assert!(
        body_str.contains("/poll/"),
        "expected the retry poll script, got: {body_str}"
    );
    assert!(
        !body_str.contains("kernel http"),
        "must not serve a boot script for a missing override target"
    );
}

async fn http_get(port: u16, path: &str) -> (String, Vec<u8>) {
    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, body) = parse_http_response(&response);
    (status, body)
}

#[tokio::test]
async fn test_static_served_from_configured_dir() {
    // Static serving must resolve against the configured `static_dir`,
    // independent of the process CWD (issue 001).
    let tmp = tempdir().unwrap();
    let static_dir = tmp.path().join("assets");
    fs::create_dir_all(&static_dir).unwrap();
    fs::write(static_dir.join("hello.txt"), b"STATIC-OK").unwrap();

    let port: u16 = 26107;
    let server_config = ServerConfig {
        http_bind: format!("127.0.0.1:{port}"),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: tmp.path().to_path_buf(),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: tmp.path().join("cache"),
        static_dir: static_dir.clone(),
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
    let _leaked = Box::leak(Box::new(tmp));

    let server_store = state_store.clone();
    let server_config_clone = shared_config.clone();
    tokio::spawn(async move {
        let _ = bootycall_http::run_http_server(
            &format!("127.0.0.1:{port}"),
            server_config_clone,
            server_store,
        )
        .await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let (status, body) = http_get(port, "/static/hello.txt").await;
    assert!(
        status.contains("200"),
        "configured static file must serve, got: {status}"
    );
    // Body is streamed (chunked transfer-encoding), so match a substring.
    assert!(
        String::from_utf8_lossy(&body).contains("STATIC-OK"),
        "served static body must contain the file content"
    );
}

#[tokio::test]
async fn test_health_endpoint_reflects_cache_readiness() {
    // Degraded until every configured host has non-empty cached artifacts,
    // then healthy (issue 032).
    let port: u16 = 26105;
    let (config, _state_store) = spawn_test_server(port).await;
    let (cache_dir, host_mac) = {
        let g = config.read();
        (g.server.cache_dir.clone(), g.hosts[0].mac.clone())
    };

    // Initially degraded — no cached kernel/initrd for the configured host.
    let (status, body) = http_get(port, "/api/health").await;
    assert!(
        status.contains("503"),
        "missing artifacts must be degraded, got: {status}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["hosts_total"], 1);
    assert_eq!(json["hosts_not_ready_count"], 1);
    // No api_token configured → the endpoint stays fully open and the
    // not-ready names are still disclosed (matches the /api/status posture).
    assert_eq!(json["hosts_not_ready"], serde_json::json!(["nixos-test"]));

    // Provision non-empty kernel + initrd, then it must be healthy.
    let host_cache = cache_dir.join(&host_mac);
    fs::create_dir_all(&host_cache).unwrap();
    fs::write(host_cache.join("kernel"), b"KERNELBYTES").unwrap();
    fs::write(host_cache.join("initrd"), b"INITRDBYTES").unwrap();

    let (status, body) = http_get(port, "/api/health").await;
    assert!(
        status.contains("200"),
        "present artifacts must be healthy, got: {status}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "healthy");
    assert_eq!(json["hosts_not_ready_count"], 0);
}

/// GET `path` with an `X-API-Token` header, returning (status line, body).
async fn http_get_with_token(port: u16, path: &str, token: &str) -> (String, Vec<u8>) {
    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    client
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-API-Token: {token}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, body) = parse_http_response(&response);
    (status, body)
}

#[tokio::test]
async fn test_health_hides_host_names_from_unauthenticated_callers() {
    // Issue 086: with an api_token configured, the open readiness probe must
    // keep working (status + counts, correct HTTP code) but must not disclose
    // configured host names; the `hosts_not_ready` list only appears when the
    // caller presents the valid token.
    let port: u16 = 26111;
    let (config, _state_store) = spawn_test_server(port).await;
    {
        let mut guard = config.write();
        guard.server.api_token = Some("health-secret".to_string());
    }

    // Unauthenticated: still a usable probe (503 while degraded, never 401),
    // but counts only — no host names anywhere in the body.
    let (status, body) = http_get(port, "/api/health").await;
    assert!(
        status.contains("503"),
        "probe must stay usable without a token, got: {status}"
    );
    let body_str = String::from_utf8_lossy(&body).to_string();
    assert!(
        !body_str.contains("nixos-test"),
        "unauthenticated health body must not leak host names, got: {body_str}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "degraded");
    assert_eq!(json["hosts_total"], 1);
    assert_eq!(json["hosts_not_ready_count"], 1);
    assert!(
        json.get("hosts_not_ready").is_none(),
        "unauthenticated health body must omit hosts_not_ready, got: {body_str}"
    );

    // A wrong token is treated exactly like no token: probe still answers,
    // names still withheld.
    let (status, body) = http_get_with_token(port, "/api/health", "wrong-token").await;
    assert!(
        status.contains("503"),
        "probe must stay usable with a bad token, got: {status}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("hosts_not_ready").is_none(),
        "bad-token health body must omit hosts_not_ready, got: {json}"
    );

    // Valid token: the not-ready host-name list is included.
    let (status, body) = http_get_with_token(port, "/api/health", "health-secret").await;
    assert!(status.contains("503"), "still degraded, got: {status}");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["hosts_not_ready_count"], 1);
    assert_eq!(json["hosts_not_ready"], serde_json::json!(["nixos-test"]));
}

async fn http_get_with_host(port: u16, path: &str, host_header: &str) -> (String, String) {
    let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    let (status, _, body) = parse_http_response(&response);
    (status, String::from_utf8_lossy(&body).to_string())
}

#[tokio::test]
async fn test_spoofed_host_header_not_reflected_in_boot_urls() {
    // Issue 069: a client-supplied Host header must not control the
    // kernel/initrd/chain URLs in generated boot scripts — behind a
    // path-keyed caching proxy that would let an attacker poison the boot
    // script other PXE clients receive.
    let port: u16 = 26110;
    let (config, _state_store) = spawn_test_server(port).await;
    {
        let mut guard = config.write();
        guard.server.allowed_hosts = vec!["127.0.0.1".to_string()];
    }

    // An allowlisted Host (port stripped before validation) is still honoured.
    {
        let (status, body) = http_get_with_host(
            port,
            "/poll/aa-bb-cc-dd-ee-ff",
            &format!("127.0.0.1:{port}"),
        )
        .await;
        assert!(status.contains("200 OK"), "got: {status}");
        assert!(
            body.contains(&format!(
                "kernel http://127.0.0.1:{port}/cache/aa:bb:cc:dd:ee:ff/kernel"
            )),
            "allowlisted Host must be reflected, got: {body}"
        );
    }

    // A spoofed Host must be replaced by the first allowed host + http_bind
    // port and must not appear anywhere in the rendered script.
    {
        let (status, body) =
            http_get_with_host(port, "/poll/aa-bb-cc-dd-ee-ff", "attacker.example:8080").await;
        assert!(status.contains("200 OK"), "got: {status}");
        assert!(
            !body.contains("attacker.example"),
            "spoofed Host must not be reflected into boot URLs, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "kernel http://127.0.0.1:{port}/cache/aa:bb:cc:dd:ee:ff/kernel"
            )),
            "kernel URL must use the trusted fallback host, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "initrd http://127.0.0.1:{port}/cache/aa:bb:cc:dd:ee:ff/initrd"
            )),
            "initrd URL must use the trusted fallback host, got: {body}"
        );
    }

    // /start builds its chain URL the same way.
    {
        let (status, body) = http_get_with_host(port, "/start", "attacker.example:8080").await;
        assert!(status.contains("200 OK"), "got: {status}");
        assert!(!body.contains("attacker.example"));
        assert!(body.contains(&format!("http://127.0.0.1:{port}/poll/")));
    }

    // advertised_host is authoritative: it overrides both the Host header
    // and the allowlist reflection.
    let advertised = "boot.internal:9090";
    {
        let mut guard = config.write();
        guard.server.advertised_host = Some(advertised.to_string());
    }
    {
        let (status, body) =
            http_get_with_host(port, "/poll/aa-bb-cc-dd-ee-ff", "attacker.example:8080").await;
        assert!(status.contains("200 OK"), "got: {status}");
        assert!(!body.contains("attacker.example"));
        assert!(
            body.contains(&format!(
                "kernel http://{advertised}/cache/aa:bb:cc:dd:ee:ff/kernel"
            )),
            "kernel URL must use advertised_host, got: {body}"
        );
        assert!(
            body.contains(&format!(
                "initrd http://{advertised}/cache/aa:bb:cc:dd:ee:ff/initrd"
            )),
            "initrd URL must use advertised_host, got: {body}"
        );
    }
}

#[tokio::test]
async fn test_firmware_boot_entry_paths_serve_start_script() {
    // Issue 081: the embedded iPXE bootstrap (packages/ipxe/default.nix)
    // chainloads http://<server>[:port]/start on first boot, and firmware
    // flashed before that fix chainloads http://<server>/ipxe/config.ipxe.
    // Both first-request paths must resolve to a real endpoint (no 404)
    // that serves the /start entry script.
    let port: u16 = 26112;
    let (_config, _state_store) = spawn_test_server(port).await;

    for path in ["/start", "/ipxe/config.ipxe"] {
        let (status, body) = http_get(port, path).await;
        assert!(
            status.contains("200 OK"),
            "firmware boot entry {path} must serve, got: {status}"
        );
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.starts_with("#!ipxe"),
            "{path} must serve an iPXE script, got: {body_str}"
        );
        assert!(
            body_str.contains(&format!(
                "chain --autofree --replace http://127.0.0.1:{port}/poll/${{mac:hexhyp}}"
            )),
            "{path} must chain to the poll endpoint, got: {body_str}"
        );
    }
}

#[tokio::test]
async fn test_read_apis_require_token_when_configured() {
    // With api_token set, /api/status and /api/logs must 401 without the header
    // and 200 with it (issue 037). The unauthenticated-open case is covered by
    // test_api_status_endpoint / test_api_logs_endpoint (which set no token).
    let port: u16 = 26104;
    let (config, _state_store) = spawn_test_server(port).await;
    {
        let mut guard = config.write();
        guard.server.api_token = Some("read-secret".to_string());
    }

    for path in ["/api/status", "/api/logs"] {
        // No token → 401
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let (status, _, _) = parse_http_response(&response);
        assert!(
            status.contains("401"),
            "{path} without token must be 401, got: {status}"
        );

        // Correct token → 200
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        client
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-API-Token: read-secret\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let (status, _, _) = parse_http_response(&response);
        assert!(
            status.contains("200"),
            "{path} with correct token must be 200, got: {status}"
        );
    }
}
