use crate::error::CoreError;
use bootycall_log::{info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_oled_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub http_bind: String,
    pub tftp_bind: String,
    pub tftp_root: PathBuf,
    pub proxy_dhcp_bind: String,
    pub cache_dir: PathBuf,
    pub default_bootloader_amd64: String,
    pub default_bootloader_arm64: String,
    #[serde(default = "default_oled_enabled")]
    pub oled_enabled: bool,
    /// Shared secret required on mutating dashboard endpoints
    /// (POST /api/override). When absent, mutating endpoints run
    /// unauthenticated — same behaviour as before P1-7. Populate this
    /// (or bind the dashboard behind a reverse proxy) before exposing
    /// the box beyond localhost.
    #[serde(default)]
    pub api_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostConfig {
    pub mac: String,
    pub name: String,
    pub image_path: PathBuf,
    pub bootloader: Option<String>,
    pub kernel_path: Option<String>,
    pub initrd_path: Option<String>,
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub hosts: Vec<HostConfig>,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, CoreError> {
        let file_content = std::fs::read_to_string(path)?;
        let mut config: Config = serde_yaml::from_str(&file_content)?;

        // Normalize MAC addresses to lowercase
        for host in &mut config.hosts {
            host.mac = crate::mac::normalize_mac(&host.mac);
        }

        // IMP-A: reject bad configs at load rather than letting a typo surface
        // late as an obscure bind failure or a host that silently never
        // matches. The watcher reload path also goes through `load`, so a bad
        // hot-reload is rejected and the previous good config keeps serving.
        config.validate()?;

        Ok(config)
    }

    /// Validate a freshly parsed (and MAC-normalised) config.
    ///
    /// Checks, with per-field error messages naming the offending value/host:
    /// - the three bind fields parse as `std::net::SocketAddr`;
    /// - `default_bootloader_amd64` / `_arm64` are non-empty;
    /// - each host MAC is a valid normalised MAC;
    /// - host MACs and host names are unique;
    /// - `api_token`, when set, is non-empty (an empty token would
    ///   authenticate a caller sending an empty `X-API-Token` header).
    pub fn validate(&self) -> Result<(), CoreError> {
        use std::collections::HashSet;
        use std::net::SocketAddr;

        let invalid = |msg: String| CoreError::InvalidConfig(msg);

        // Bind addresses must parse as a concrete host:port socket address.
        for (field, value) in [
            ("http_bind", &self.server.http_bind),
            ("tftp_bind", &self.server.tftp_bind),
            ("proxy_dhcp_bind", &self.server.proxy_dhcp_bind),
        ] {
            if value.parse::<SocketAddr>().is_err() {
                return Err(invalid(format!(
                    "server.{field} = {value:?} is not a valid socket address (expected e.g. \"0.0.0.0:69\")"
                )));
            }
        }

        // Default bootloaders must be non-empty (a blank BootfileName would be
        // handed to PXE clients).
        for (field, value) in [
            (
                "default_bootloader_amd64",
                &self.server.default_bootloader_amd64,
            ),
            (
                "default_bootloader_arm64",
                &self.server.default_bootloader_arm64,
            ),
        ] {
            if value.trim().is_empty() {
                return Err(invalid(format!("server.{field} must not be empty")));
            }
        }

        // An empty api_token would authenticate an empty header — reject it.
        if let Some(token) = &self.server.api_token
            && token.is_empty()
        {
            return Err(invalid(
                "server.api_token is set but empty; unset it to disable auth or provide a real secret"
                    .to_string(),
            ));
        }

        // Per-host: valid MAC syntax, and no duplicate MAC or name.
        let mut seen_macs: HashSet<&str> = HashSet::new();
        let mut seen_names: HashSet<&str> = HashSet::new();
        for host in &self.hosts {
            if !crate::mac::is_valid_mac(&host.mac) {
                return Err(invalid(format!(
                    "host {:?} has an invalid MAC address {:?} (expected aa:bb:cc:dd:ee:ff)",
                    host.name, host.mac
                )));
            }
            if !seen_macs.insert(host.mac.as_str()) {
                return Err(invalid(format!(
                    "duplicate host MAC {:?} (each host MAC must be unique)",
                    host.mac
                )));
            }
            if host.name.trim().is_empty() {
                return Err(invalid("a host has an empty name".to_string()));
            }
            if !seen_names.insert(host.name.as_str()) {
                return Err(invalid(format!(
                    "duplicate host name {:?} (each host name must be unique)",
                    host.name
                )));
            }
        }

        Ok(())
    }

    pub fn find_host(&self, mac: &str) -> Option<&HostConfig> {
        let normalized = crate::mac::normalize_mac(mac);
        self.hosts.iter().find(|h| h.mac == normalized)
    }
}

pub fn watch_config<F>(
    path: PathBuf,
    mut on_reload: F,
) -> Result<notify::RecommendedWatcher, CoreError>
where
    F: FnMut(Config) + Send + 'static,
{
    use notify::{Config as WatcherConfig, RecommendedWatcher, Watcher};
    use std::sync::mpsc::{RecvTimeoutError, channel};

    let (tx, rx) = channel();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            if let Err(e) = tx.send(res) {
                warn!("Failed to send config watch event: {:?}", e);
            }
        },
        WatcherConfig::default(),
    )?;

    // Watch the *parent directory*, not the file itself. Editors that save
    // atomically (`vim`, `sed -i`, Ansible) rename a new file over the
    // original; a watch bound to the old inode goes silent after the first
    // save. Filtering by the target file name gives us the same signal
    // without depending on the inode.
    let watch_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let watch_name = path.file_name().map(std::ffi::OsString::from);

    watcher.watch(&watch_dir, notify::RecursiveMode::NonRecursive)?;

    std::thread::spawn(move || {
        let debounce = std::time::Duration::from_millis(200);

        loop {
            // Block until at least one event arrives; if the channel closes,
            // the watcher is gone and we can exit the reload thread.
            let first = match rx.recv() {
                Ok(res) => res,
                Err(_) => return,
            };

            let mut dirty = event_targets_config(&first, watch_name.as_deref());

            // Drain follow-up events (typical for an atomic
            // save: remove + create + modify all fire within a few ms).
            loop {
                match rx.recv_timeout(debounce) {
                    Ok(res) => {
                        if event_targets_config(&res, watch_name.as_deref()) {
                            dirty = true;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }

            if !dirty {
                continue;
            }

            info!("Configuration file changed, reloading...");
            let load_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match Config::load(&path) {
                    Ok(config) => Some(config),
                    Err(e) => {
                        warn!("Failed to reload configuration: {:?}", e);
                        None
                    }
                }
            }));
            let config = match load_result {
                Ok(Some(cfg)) => cfg,
                Ok(None) => continue,
                Err(_) => {
                    warn!("Panic while reading config; keeping previous config live");
                    continue;
                }
            };

            // A panic inside on_reload used to kill the watcher thread and
            // freeze reloads for the rest of the process lifetime.
            let cb_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                on_reload(config);
            }));
            if cb_result.is_err() {
                warn!("Config reload callback panicked; watcher stays alive");
            }
        }
    });

    Ok(watcher)
}

fn event_targets_config(
    res: &Result<notify::Event, notify::Error>,
    watch_name: Option<&std::ffi::OsStr>,
) -> bool {
    use notify::EventKind;

    let event = match res {
        Ok(e) => e,
        Err(e) => {
            warn!("Config watcher channel error: {:?}", e);
            return false;
        }
    };

    // Ignore remove-only events (transient during atomic replace); reload
    // on Modify or Create — the new inode surfaces as Create when the
    // temporary is renamed on top of the config file.
    match event.kind {
        EventKind::Modify(_) | EventKind::Create(_) => {}
        _ => return false,
    }

    let Some(name) = watch_name else {
        return true;
    };

    // Only fire when the event paths actually mention our target file —
    // parent-directory watches otherwise flap on every sibling write.
    event
        .paths
        .iter()
        .any(|p| p.file_name().map(|f| f == name).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_config_load_and_normalization() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");
        let mut file = File::create(&file_path).unwrap();

        let yaml = r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "52-54-00-10-10-10"
    name: "test-host"
    image_path: "/tmp/nixos.iso"
"#;
        file.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(&file_path).unwrap();
        assert_eq!(config.server.http_bind, "0.0.0.0:8080");
        assert_eq!(config.hosts.len(), 1);
        // MAC address should be normalized with colons and lowercase
        assert_eq!(config.hosts[0].mac, "52:54:00:10:10:10");

        // Test finding host
        let found = config.find_host("52:54:00:10:10:10").unwrap();
        assert_eq!(found.name, "test-host");

        let found_hyphens = config.find_host("52-54-00-10-10-10").unwrap();
        assert_eq!(found_hyphens.name, "test-host");
    }

    #[test]
    fn test_config_load_missing_file() {
        let result = Config::load("/tmp/nonexistent_bootycall_config_42.yaml");
        assert!(
            result.is_err(),
            "Loading a nonexistent file should return Err"
        );
    }

    #[test]
    fn test_config_load_invalid_yaml() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bad.yaml");
        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"{{{{not valid yaml at all::::").unwrap();

        let result = Config::load(&file_path);
        assert!(result.is_err(), "Loading invalid YAML should return Err");
    }

    #[test]
    fn test_find_host_not_found() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");
        let mut file = File::create(&file_path).unwrap();

        let yaml = r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "52:54:00:10:10:10"
    name: "test-host"
    image_path: "/tmp/nixos.iso"
"#;
        file.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(&file_path).unwrap();
        assert!(
            config.find_host("ff:ff:ff:ff:ff:ff").is_none(),
            "Searching for an unknown MAC should return None"
        );
    }

    #[test]
    fn test_find_host_case_insensitive() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");
        let mut file = File::create(&file_path).unwrap();

        let yaml = r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "aa:bb:cc:dd:ee:ff"
    name: "lower-host"
    image_path: "/tmp/nixos.iso"
"#;
        file.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(&file_path).unwrap();
        // Lookup with uppercase MAC should still find the host
        let found = config.find_host("AA:BB:CC:DD:EE:FF");
        assert!(
            found.is_some(),
            "Uppercase MAC lookup should match lowercase entry"
        );
        assert_eq!(found.unwrap().name, "lower-host");
    }

    #[test]
    fn test_config_empty_hosts() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");
        let mut file = File::create(&file_path).unwrap();

        let yaml = r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts: []
"#;
        file.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(&file_path).unwrap();
        assert_eq!(
            config.hosts.len(),
            0,
            "Empty hosts list should load as zero-length vec"
        );
        assert!(config.find_host("aa:bb:cc:dd:ee:ff").is_none());
    }

    fn minimal_yaml(mac: &str) -> String {
        format!(
            r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "{mac}"
    name: "watch-host"
    image_path: "/tmp/x.iso"
"#
        )
    }

    fn wait_for_mac(rx: &std::sync::mpsc::Receiver<Config>, expected: &str) -> Option<Config> {
        // The debounce inside watch_config is ~200ms; poll for up to 3s.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(cfg) if cfg.hosts.first().map(|h| h.mac.as_str()) == Some(expected) => {
                    return Some(cfg);
                }
                Ok(_) => continue, // stale reload, keep waiting
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    }

    #[test]
    fn test_watch_reload_on_atomic_replace() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootycall.yaml");
        std::fs::write(&path, minimal_yaml("aa:aa:aa:aa:aa:aa")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch_config(path.clone(), move |cfg| {
            let _ = tx.send(cfg);
        })
        .expect("watch_config");

        // Give the watcher a moment to arm.
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Atomic replace: write to a sibling temp, then rename onto the config.
        let tmp = dir.path().join("bootycall.yaml.tmp");
        std::fs::write(&tmp, minimal_yaml("bb:bb:bb:bb:bb:bb")).unwrap();
        std::fs::rename(&tmp, &path).expect("rename");

        let cfg = wait_for_mac(&rx, "bb:bb:bb:bb:bb:bb")
            .expect("reload after atomic replace within deadline");
        assert_eq!(cfg.hosts[0].mac, "bb:bb:bb:bb:bb:bb");
    }

    #[test]
    fn test_watch_reload_on_in_place_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootycall.yaml");
        std::fs::write(&path, minimal_yaml("aa:aa:aa:aa:aa:aa")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let _watcher = watch_config(path.clone(), move |cfg| {
            let _ = tx.send(cfg);
        })
        .expect("watch_config");

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Plain overwrite of the same file (no rename).
        std::fs::write(&path, minimal_yaml("cc:cc:cc:cc:cc:cc")).unwrap();

        let cfg = wait_for_mac(&rx, "cc:cc:cc:cc:cc:cc")
            .expect("reload after in-place write within deadline");
        assert_eq!(cfg.hosts[0].mac, "cc:cc:cc:cc:cc:cc");
    }

    #[test]
    fn test_config_multiple_hosts() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");
        let mut file = File::create(&file_path).unwrap();

        let yaml = r#"
server:
  http_bind: "0.0.0.0:8080"
  tftp_bind: "0.0.0.0:69"
  tftp_root: "./tftpboot"
  proxy_dhcp_bind: "0.0.0.0:4011"
  cache_dir: "./cache"
  default_bootloader_amd64: "boot/x64/ipxe.efi"
  default_bootloader_arm64: "boot/arm64/ipxe.efi"

hosts:
  - mac: "11:22:33:44:55:01"
    name: "host-alpha"
    image_path: "/tmp/alpha.iso"
  - mac: "11:22:33:44:55:02"
    name: "host-beta"
    image_path: "/tmp/beta.iso"
  - mac: "11:22:33:44:55:03"
    name: "host-gamma"
    image_path: "/tmp/gamma.iso"
"#;
        file.write_all(yaml.as_bytes()).unwrap();

        let config = Config::load(&file_path).unwrap();
        assert_eq!(config.hosts.len(), 3);

        // Find each host by its MAC
        let alpha = config.find_host("11:22:33:44:55:01").unwrap();
        assert_eq!(alpha.name, "host-alpha");

        let beta = config.find_host("11:22:33:44:55:02").unwrap();
        assert_eq!(beta.name, "host-beta");

        let gamma = config.find_host("11:22:33:44:55:03").unwrap();
        assert_eq!(gamma.name, "host-gamma");
    }
}
