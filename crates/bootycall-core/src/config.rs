use crate::error::CoreError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub http_bind: String,
    pub tftp_bind: String,
    pub tftp_root: PathBuf,
    pub proxy_dhcp_bind: String,
    pub cache_dir: PathBuf,
    pub default_bootloader_amd64: String,
    pub default_bootloader_arm64: String,
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
            host.mac = host.mac.to_ascii_lowercase().replace('-', ":");
        }

        Ok(config)
    }

    pub fn find_host(&self, mac: &str) -> Option<&HostConfig> {
        let normalized = mac.to_ascii_lowercase().replace('-', ":");
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
    use notify::{Config as WatcherConfig, EventKind, RecommendedWatcher, Watcher};
    use std::sync::mpsc::channel;

    let (tx, rx) = channel();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            if let Err(e) = tx.send(res) {
                warn!("Failed to send config watch event: {:?}", e);
            }
        },
        WatcherConfig::default(),
    )?;

    watcher.watch(&path, notify::RecursiveMode::NonRecursive)?;

    // Spawn block for reading channel events
    std::thread::spawn(move || {
        for res in rx {
            match res {
                Ok(event) => {
                    if let EventKind::Modify(_) = event.kind {
                        info!("Configuration file modified, reloading...");
                        // Sleep briefly to allow filesystem writes to complete cleanly
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        match Config::load(&path) {
                            Ok(config) => on_reload(config),
                            Err(e) => warn!("Failed to reload configuration: {:?}", e),
                        }
                    }
                }
                Err(e) => warn!("Config watcher channel error: {:?}", e),
            }
        }
    });

    Ok(watcher)
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
        assert!(result.is_err(), "Loading a nonexistent file should return Err");
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
        assert!(found.is_some(), "Uppercase MAC lookup should match lowercase entry");
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
        assert_eq!(config.hosts.len(), 0, "Empty hosts list should load as zero-length vec");
        assert!(config.find_host("aa:bb:cc:dd:ee:ff").is_none());
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
