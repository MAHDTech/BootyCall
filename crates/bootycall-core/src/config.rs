use crate::error::CoreError;
use bootycall_log::{info, warn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Quiet window the hot-reload watcher waits for after a config-file event
/// before reloading. Atomic saves (`vim`, `sed -i`, Ansible) fire a burst of
/// remove/create/modify events within a few milliseconds; debouncing collapses
/// the burst into a single reload.
const CONFIG_DEBOUNCE_DURATION: std::time::Duration = std::time::Duration::from_millis(200);

fn default_oled_enabled() -> bool {
    true
}

fn default_oled_brightness() -> u8 {
    255
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("./static")
}

fn default_bootloader_bios() -> String {
    // iPXE's undionly NBP is a real-mode network bootstrap that legacy BIOS
    // option ROMs (PXE architecture 0) can execute; an EFI image cannot run
    // there. Keeps existing configs working when the key is omitted.
    "boot/x64/undionly.kpxe".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// IP address and port to bind the HTTP dashboard and dynamic API endpoints.
    pub http_bind: String,
    /// IP address and port to bind the TFTP server serving bootloader binaries.
    pub tftp_bind: String,
    /// Local filesystem path to the root directory for TFTP transfers.
    pub tftp_root: PathBuf,
    /// IP address and port to bind the Proxy DHCP service.
    pub proxy_dhcp_bind: String,
    /// Local filesystem path to the directory used for caching extracted ISO kernels and initrds.
    pub cache_dir: PathBuf,
    /// Root directory for static HTTP assets (wallpapers, etc.). Resolved
    /// independently of the process CWD; defaults to `./static` so existing
    /// configs keep working. All static/wallpaper file access is routed through
    /// `safe_join` against this root.
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
    pub default_bootloader_amd64: String,
    pub default_bootloader_arm64: String,
    /// Bootloader served to legacy BIOS PXE clients (Option 93 architecture 0),
    /// which cannot execute the EFI `default_bootloader_amd64` image. Defaults
    /// to `boot/x64/undionly.kpxe`; override to match your tftp layout.
    #[serde(default = "default_bootloader_bios")]
    pub default_bootloader_bios: String,
    #[serde(default = "default_oled_enabled")]
    pub oled_enabled: bool,
    /// OLED panel brightness (0–255, default full). Scales the grayscale→RGB565
    /// LUT; lower values dim the display (and reduce burn-in/power).
    #[serde(default = "default_oled_brightness")]
    pub oled_brightness: u8,
    /// Shared secret required on mutating dashboard endpoints
    /// (POST /api/override). When absent, mutating endpoints run
    /// unauthenticated — same behaviour as before P1-7. Populate this
    /// (or bind the dashboard behind a reverse proxy) before exposing
    /// the box beyond localhost.
    #[serde(default)]
    pub api_token: Option<String>,
    /// Upper bound, in bytes, on a single extracted kernel/initrd artifact.
    /// `None` (the default) means unbounded. A crafted or genuinely huge
    /// initramfs can otherwise fill the appliance's small eMMC and take down
    /// every service (single binary) — see issue 006.
    #[serde(default)]
    pub max_artifact_bytes: Option<u64>,
    /// Authoritative `host[:port]` advertised to PXE clients inside generated
    /// iPXE boot scripts (the kernel/initrd/chain URLs). When set, generated
    /// URLs always use this value and the client-supplied `Host:` header is
    /// ignored — reflecting that header lets an attacker behind a path-keyed
    /// caching proxy poison the boot script served to *other* clients
    /// (issue 069). Unset by default so existing configs keep working.
    #[serde(default)]
    pub advertised_host: Option<String>,
    /// Allowlist of hostnames/IP addresses (compared with any `:port`
    /// stripped) that the client-supplied `Host:` header may reflect into
    /// generated boot-script URLs when `advertised_host` is unset. A header
    /// whose host part is not listed is replaced by the first entry plus the
    /// `http_bind` port. Empty (the default) preserves the legacy
    /// reflect-the-header behaviour for existing deployments.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// The hardware MAC address of the target host, normalized to lowercase with colons.
    pub mac: String,
    /// A unique human-readable hostname or identifier for the target host.
    pub name: String,
    /// Path to the ISO or disk image on the local filesystem containing the target OS installation files.
    pub image_path: PathBuf,
    /// Optional path (relative to tftp_root) of a custom bootloader binary to serve to this host.
    pub bootloader: Option<String>,
    /// Optional custom kernel path within the image (or absolute path) to override extraction.
    pub kernel_path: Option<String>,
    /// Optional custom initrd path within the image (or absolute path) to override extraction.
    pub initrd_path: Option<String>,
    /// Optional kernel command line arguments/parameters appended during boot.
    pub cmdline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub hosts: Vec<HostConfig>,
}

impl Config {
    /// The generic `path` is skipped (no `Debug` bound on `P`) and recorded
    /// as a display field instead.
    #[tracing::instrument(skip(path), fields(path = %path.as_ref().display()))]
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
    /// - `default_bootloader_amd64` / `_arm64` / `_bios` are non-empty;
    /// - each host MAC is a valid normalised MAC;
    /// - host MACs and host names are unique (names are compared after
    ///   trimming, so `"web"` and `"web "` count as duplicates);
    /// - `api_token`, when set, is non-empty (an empty token would
    ///   authenticate a caller sending an empty `X-API-Token` header);
    /// - `advertised_host`, when set, and every `allowed_hosts` entry are
    ///   non-empty and whitespace-free (they are interpolated into iPXE
    ///   scripts, so an embedded newline would inject script lines).
    pub fn validate(&self) -> Result<(), CoreError> {
        use std::collections::HashSet;
        use std::net::SocketAddr;

        // Bind addresses must parse as a concrete host:port socket address.
        for (field, value) in [
            ("http_bind", &self.server.http_bind),
            ("tftp_bind", &self.server.tftp_bind),
            ("proxy_dhcp_bind", &self.server.proxy_dhcp_bind),
        ] {
            if value.parse::<SocketAddr>().is_err() {
                return Err(CoreError::InvalidBindAddr {
                    field: field.to_string(),
                    value: value.clone(),
                });
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
            (
                "default_bootloader_bios",
                &self.server.default_bootloader_bios,
            ),
        ] {
            if value.trim().is_empty() {
                return Err(CoreError::EmptyBootloader(field.to_string()));
            }
        }

        // An empty api_token would authenticate an empty header — reject it.
        if let Some(token) = &self.server.api_token
            && token.is_empty()
        {
            return Err(CoreError::EmptyApiToken);
        }

        // advertised_host and allowed_hosts entries are interpolated into
        // generated iPXE scripts: reject empties (would render URLs with an
        // empty host) and whitespace (a newline would inject script lines).
        if let Some(advertised) = &self.server.advertised_host
            && (advertised.is_empty() || advertised.chars().any(char::is_whitespace))
        {
            return Err(CoreError::InvalidAdvertisedHost {
                value: advertised.clone(),
            });
        }
        for host in &self.server.allowed_hosts {
            if host.is_empty() || host.chars().any(char::is_whitespace) {
                return Err(CoreError::InvalidAllowedHost {
                    value: host.clone(),
                });
            }
        }

        // A zero artifact ceiling would reject every extraction — almost
        // certainly a mistake; unset it for "unbounded".
        if self.server.max_artifact_bytes == Some(0) {
            return Err(CoreError::ZeroArtifactCap);
        }

        // Per-host: valid MAC syntax, and no duplicate MAC or name.
        let mut seen_macs: HashSet<&str> = HashSet::new();
        let mut seen_names: HashSet<&str> = HashSet::new();
        for host in &self.hosts {
            if !crate::mac::is_valid_mac(&host.mac) {
                return Err(CoreError::InvalidMac {
                    host: host.name.clone(),
                    mac: host.mac.clone(),
                });
            }
            if !seen_macs.insert(host.mac.as_str()) {
                return Err(CoreError::DuplicateMac(host.mac.clone()));
            }
            // Key uniqueness on the *trimmed* name: "web" and "web " would
            // otherwise pass as two distinct hosts and confuse the dashboard.
            let name = host.name.trim();
            if name.is_empty() {
                return Err(CoreError::EmptyHostName);
            }
            if host.name.contains('\n') || host.name.contains('\r') {
                return Err(CoreError::InvalidHostName {
                    value: host.name.clone(),
                });
            }
            if let Some(cmdline) = &host.cmdline
                && (cmdline.contains('\n') || cmdline.contains('\r'))
            {
                return Err(CoreError::InvalidHostCmdline {
                    host: host.name.clone(),
                    value: cmdline.clone(),
                });
            }
            if !seen_names.insert(name) {
                return Err(CoreError::DuplicateName(name.to_string()));
            }
        }

        Ok(())
    }

    pub fn find_host(&self, mac: &str) -> Option<&HostConfig> {
        let normalized = crate::mac::normalize_mac(mac);
        self.hosts.iter().find(|h| h.mac == normalized)
    }
}

#[tracing::instrument(skip(on_reload), fields(path = %path.display()))]
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
        // Spans do not cross thread spawns; give the reload loop its own so
        // every debounce/reload record carries the watched path. The loop is
        // fully synchronous, so holding the entered guard for the thread
        // lifetime is safe.
        let _reload_span =
            tracing::info_span!("config_reload_watcher", path = %path.display()).entered();
        loop {
            // Block until at least one event arrives; if the channel closes,
            // the watcher is gone and we can exit the reload thread.
            let first = match rx.recv() {
                Ok(res) => res,
                Err(_) => return,
            };

            let mut dirty = event_targets_config(&first, watch_name.as_deref());

            // Drain follow-up events (typical for an atomic save:
            // remove + create + modify all fire within a few ms) against a
            // rolling deadline. Only events that target the config file
            // extend the window — unrelated sibling-file churn in the watched
            // directory would otherwise reset the timeout on every event and
            // starve the reload indefinitely.
            let mut deadline = std::time::Instant::now() + CONFIG_DEBOUNCE_DURATION;
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match rx.recv_timeout(remaining) {
                    Ok(res) => {
                        if event_targets_config(&res, watch_name.as_deref()) {
                            dirty = true;
                            deadline = std::time::Instant::now() + CONFIG_DEBOUNCE_DURATION;
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
  static_dir: "./static"
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
  static_dir: "./static"
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
  static_dir: "./static"
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
  static_dir: "./static"
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
  static_dir: "./static"
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
  static_dir: "./static"
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

    // --- Config::validate coverage (issue 043) ---------------------------

    fn base_server() -> ServerConfig {
        ServerConfig {
            http_bind: "0.0.0.0:8080".to_string(),
            tftp_bind: "0.0.0.0:69".to_string(),
            tftp_root: PathBuf::from("./tftpboot"),
            proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
            cache_dir: PathBuf::from("./cache"),
            static_dir: "./static".into(),
            default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
            default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
            default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
            oled_enabled: true,
            oled_brightness: 255,
            api_token: None,
            max_artifact_bytes: None,
            advertised_host: None,
            allowed_hosts: Vec::new(),
        }
    }

    fn host(mac: &str, name: &str) -> HostConfig {
        HostConfig {
            mac: mac.to_string(),
            name: name.to_string(),
            image_path: PathBuf::from("/tmp/x.iso"),
            bootloader: None,
            kernel_path: None,
            initrd_path: None,
            cmdline: None,
        }
    }

    fn valid_config() -> Config {
        Config {
            server: base_server(),
            hosts: vec![
                host("aa:bb:cc:dd:ee:01", "host-a"),
                host("aa:bb:cc:dd:ee:02", "host-b"),
            ],
        }
    }

    #[test]
    fn validate_accepts_a_good_config() {
        assert!(valid_config().validate().is_ok());
        // A None api_token and zero hosts are both fine.
        let mut cfg = valid_config();
        cfg.hosts.clear();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_bind_address() {
        let mut cfg = valid_config();
        cfg.server.http_bind = "localhost".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidBindAddr { ref field, ref value })
                if field == "http_bind" && value == "localhost"
        ));

        let mut cfg = valid_config();
        cfg.server.tftp_bind = "0.0.0.0:69 ".to_string(); // trailing space
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidBindAddr { ref field, ref value })
                if field == "tftp_bind" && value == "0.0.0.0:69 "
        ));

        let mut cfg = valid_config();
        cfg.server.proxy_dhcp_bind = "not-an-addr".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidBindAddr { ref field, ref value })
                if field == "proxy_dhcp_bind" && value == "not-an-addr"
        ));
    }

    #[test]
    fn validate_rejects_malformed_mac() {
        let mut cfg = valid_config();
        cfg.hosts[0].mac = "zz:bb:cc:dd:ee:ff".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidMac { ref host, ref mac })
                if host == "host-a" && mac == "zz:bb:cc:dd:ee:ff"
        ));
    }

    #[test]
    fn validate_rejects_duplicate_mac() {
        let mut cfg = valid_config();
        cfg.hosts[1].mac = cfg.hosts[0].mac.clone();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::DuplicateMac(ref mac)) if mac == &cfg.hosts[0].mac
        ));
    }

    #[test]
    fn validate_rejects_duplicate_name() {
        let mut cfg = valid_config();
        cfg.hosts[1].name = cfg.hosts[0].name.clone();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::DuplicateName(ref name)) if name == &cfg.hosts[0].name
        ));
    }

    #[test]
    fn validate_rejects_trimmed_duplicate_name() {
        // "web" and "web " must collide: uniqueness keys on the trimmed name.
        let mut cfg = valid_config();
        cfg.hosts[0].name = "web".to_string();
        cfg.hosts[1].name = "web ".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::DuplicateName(ref name)) if name == "web"
        ));

        // Leading whitespace collides too.
        let mut cfg = valid_config();
        cfg.hosts[0].name = " web".to_string();
        cfg.hosts[1].name = "web".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::DuplicateName(ref name)) if name == "web"
        ));
    }

    #[test]
    fn validate_rejects_empty_bootloader() {
        let mut cfg = valid_config();
        cfg.server.default_bootloader_amd64 = String::new();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::EmptyBootloader(ref field)) if field == "default_bootloader_amd64"
        ));

        let mut cfg = valid_config();
        cfg.server.default_bootloader_arm64 = "   ".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::EmptyBootloader(ref field)) if field == "default_bootloader_arm64"
        ));
    }

    #[test]
    fn validate_rejects_empty_api_token() {
        let mut cfg = valid_config();
        cfg.server.api_token = Some(String::new());
        assert!(matches!(cfg.validate(), Err(CoreError::EmptyApiToken)));
        // A real secret is accepted.
        cfg.server.api_token = Some("s3cr3t".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_host_name() {
        let mut cfg = valid_config();
        cfg.hosts[0].name = String::new();
        assert!(matches!(cfg.validate(), Err(CoreError::EmptyHostName)));
    }

    #[test]
    fn validate_rejects_host_name_with_newline() {
        let mut cfg = valid_config();
        cfg.hosts[0].name = "host\nname".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidHostName { ref value }) if value == "host\nname"
        ));

        let mut cfg = valid_config();
        cfg.hosts[0].name = "host\rname".to_string();
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidHostName { ref value }) if value == "host\rname"
        ));
    }

    #[test]
    fn validate_rejects_host_cmdline_with_newline() {
        let mut cfg = valid_config();
        cfg.hosts[0].cmdline = Some("console=tty0\nchain evil".to_string());
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidHostCmdline { ref host, ref value })
                if host == "host-a" && value == "console=tty0\nchain evil"
        ));

        let mut cfg = valid_config();
        cfg.hosts[0].cmdline = Some("console=tty0\rchain evil".to_string());
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidHostCmdline { ref host, ref value })
                if host == "host-a" && value == "console=tty0\rchain evil"
        ));
    }

    #[test]
    fn validate_rejects_bad_advertised_host() {
        let mut cfg = valid_config();
        cfg.server.advertised_host = Some(String::new());
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidAdvertisedHost { ref value }) if value.is_empty()
        ));

        // Whitespace would let the value inject extra iPXE script lines.
        let mut cfg = valid_config();
        cfg.server.advertised_host = Some("boot.example\nchain evil".to_string());
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidAdvertisedHost { ref value }) if value == "boot.example\nchain evil"
        ));

        // A real host:port is accepted.
        let mut cfg = valid_config();
        cfg.server.advertised_host = Some("boot.example.internal:8080".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_allowed_hosts_entry() {
        let mut cfg = valid_config();
        cfg.server.allowed_hosts = vec!["192.168.1.10".to_string(), String::new()];
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidAllowedHost { ref value }) if value.is_empty()
        ));

        let mut cfg = valid_config();
        cfg.server.allowed_hosts = vec!["boot example".to_string()];
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::InvalidAllowedHost { ref value }) if value == "boot example"
        ));

        // Plain hostnames/IPs are accepted.
        let mut cfg = valid_config();
        cfg.server.allowed_hosts = vec!["192.168.1.10".to_string(), "boot.internal".to_string()];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_max_artifact_bytes() {
        let mut cfg = valid_config();
        cfg.server.max_artifact_bytes = Some(0);
        assert!(matches!(cfg.validate(), Err(CoreError::ZeroArtifactCap)));
    }

    #[test]
    fn test_config_deny_unknown_fields() {
        // cspell:ignore tokan
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("bootycall.yaml");

        // Unknown top-level key
        {
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
unknown_top_level_field: "value"
"#;
            file.write_all(yaml.as_bytes()).unwrap();
            let result = Config::load(&file_path);
            assert!(result.is_err());
            let err_msg = format!("{:?}", result.err().unwrap());
            assert!(
                err_msg.contains("unknown field") || err_msg.contains("unknown_top_level_field"),
                "Expected unknown field error, got: {}",
                err_msg
            );
        }

        // Unknown server key (typo'd api_token)
        {
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
  api_tokan: "should_fail"
hosts: []
"#;
            file.write_all(yaml.as_bytes()).unwrap();
            let result = Config::load(&file_path);
            assert!(result.is_err());
            let err_msg = format!("{:?}", result.err().unwrap());
            assert!(
                err_msg.contains("unknown field") || err_msg.contains("api_tokan"),
                "Expected unknown field error, got: {}",
                err_msg
            );
        }

        // Unknown host key
        {
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
    unknown_host_field: "value"
"#;
            file.write_all(yaml.as_bytes()).unwrap();
            let result = Config::load(&file_path);
            assert!(result.is_err());
            let err_msg = format!("{:?}", result.err().unwrap());
            assert!(
                err_msg.contains("unknown field") || err_msg.contains("unknown_host_field"),
                "Expected unknown field error, got: {}",
                err_msg
            );
        }
    }
}
