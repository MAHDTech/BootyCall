use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostStatus {
    Polling,
    Booting,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostState {
    pub mac: String,
    pub name: Option<String>,
    pub status: HostStatus,
    pub assigned_target: Option<String>,
    pub last_seen: SystemTime,
    pub client_ip: Option<String>,
    pub architecture: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub timestamp: SystemTime,
    pub level: String,
    pub mac: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    hosts: Arc<RwLock<HashMap<String, HostState>>>,
    logs: Arc<RwLock<Vec<LogEvent>>>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    pub fn new() -> Self {
        Self {
            hosts: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn update_host_status(
        &self,
        mac: &str,
        status: HostStatus,
        name: Option<String>,
        target: Option<String>,
        ip: Option<String>,
        arch: Option<String>,
    ) {
        let mut hosts = self.hosts.write().unwrap();
        let normalized = mac.to_ascii_lowercase().replace('-', ":");
        let entry = hosts
            .entry(normalized.clone())
            .or_insert_with(|| HostState {
                mac: normalized,
                name: None,
                status,
                assigned_target: None,
                last_seen: SystemTime::now(),
                client_ip: None,
                architecture: None,
            });

        entry.status = status;
        entry.last_seen = SystemTime::now();
        if name.is_some() {
            entry.name = name;
        }
        if target.is_some() {
            entry.assigned_target = target;
        }
        if ip.is_some() {
            entry.client_ip = ip;
        }
        if arch.is_some() {
            entry.architecture = arch;
        }
    }

    pub fn get_host(&self, mac: &str) -> Option<HostState> {
        let hosts = self.hosts.read().unwrap();
        let normalized = mac.to_ascii_lowercase().replace('-', ":");
        hosts.get(&normalized).cloned()
    }

    pub fn list_hosts(&self) -> Vec<HostState> {
        let hosts = self.hosts.read().unwrap();
        hosts.values().cloned().collect()
    }

    pub fn has_recent_activity(&self, max_age: std::time::Duration) -> bool {
        let now = SystemTime::now();
        let hosts = self.hosts.read().unwrap();
        hosts.values().any(|h| {
            now.duration_since(h.last_seen)
                .map(|age| age <= max_age)
                .unwrap_or(false)
        })
    }

    pub fn log_event(&self, level: &str, mac: Option<&str>, message: &str) {
        let mut logs = self.logs.write().unwrap();
        logs.push(LogEvent {
            timestamp: SystemTime::now(),
            level: level.to_string(),
            mac: mac.map(|m| m.to_ascii_lowercase().replace('-', ":")),
            message: message.to_string(),
        });
        let len = logs.len();
        if len > 200 {
            logs.drain(0..len - 200);
        }
    }

    pub fn list_logs(&self) -> Vec<LogEvent> {
        let logs = self.logs.read().unwrap();
        logs.clone()
    }

    pub fn clean_stale_hosts(&self, max_idle_secs: u64) {
        let mut hosts = self.hosts.write().unwrap();
        let now = SystemTime::now();
        hosts.retain(|_, state| {
            if let Ok(duration) = now.duration_since(state.last_seen) {
                duration.as_secs() < max_idle_secs
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_store_operations() {
        let store = StateStore::new();

        // Check empty state
        assert_eq!(store.list_hosts().len(), 0);
        assert_eq!(store.list_logs().len(), 0);

        // Update host status
        store.update_host_status(
            "AA-BB-CC-11-22-33",
            HostStatus::Polling,
            Some("host1".to_string()),
            Some("target1".to_string()),
            Some("192.168.1.100".to_string()),
            Some("x86_64".to_string()),
        );

        // MAC address should be normalized
        let host = store.get_host("aa:bb:cc:11:22:33").unwrap();
        assert_eq!(host.name, Some("host1".to_string()));
        assert_eq!(host.status, HostStatus::Polling);
        assert_eq!(host.assigned_target, Some("target1".to_string()));
        assert_eq!(host.client_ip, Some("192.168.1.100".to_string()));
        assert_eq!(host.architecture, Some("x86_64".to_string()));

        // Log events
        store.log_event("INFO", Some("AA-BB-CC-11-22-33"), "Started polling");
        let logs = store.list_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].mac, Some("aa:bb:cc:11:22:33".to_string()));
        assert_eq!(logs[0].message, "Started polling");
    }

    #[test]
    fn test_clean_stale_hosts() {
        let store = StateStore::new();

        store.update_host_status(
            "aa:bb:cc:00:00:01",
            HostStatus::Polling,
            Some("stale-host".to_string()),
            None,
            None,
            None,
        );

        // Verify the host exists
        assert!(store.get_host("aa:bb:cc:00:00:01").is_some());

        // Sleep briefly so the host's last_seen is in the past relative to a
        // zero-second TTL
        std::thread::sleep(std::time::Duration::from_millis(50));

        // A max_idle of 0 seconds means anything older than "right now" is stale
        store.clean_stale_hosts(0);

        assert!(
            store.get_host("aa:bb:cc:00:00:01").is_none(),
            "Stale host should be removed after clean_stale_hosts(0)"
        );
        assert_eq!(store.list_hosts().len(), 0);
    }

    #[test]
    fn test_partial_update_preserves_existing() {
        let store = StateStore::new();

        // First update sets all fields
        store.update_host_status(
            "aa:bb:cc:00:00:02",
            HostStatus::Booting,
            Some("my-host".to_string()),
            Some("target-a".to_string()),
            Some("10.0.0.1".to_string()),
            Some("aarch64".to_string()),
        );

        // Second update passes None for optional fields — originals should survive
        store.update_host_status(
            "aa:bb:cc:00:00:02",
            HostStatus::Completed,
            None,
            None,
            None,
            None,
        );

        let host = store.get_host("aa:bb:cc:00:00:02").unwrap();
        assert_eq!(
            host.status,
            HostStatus::Completed,
            "Status should be updated"
        );
        assert_eq!(
            host.name,
            Some("my-host".to_string()),
            "Name should be preserved"
        );
        assert_eq!(
            host.assigned_target,
            Some("target-a".to_string()),
            "Target should be preserved"
        );
        assert_eq!(
            host.client_ip,
            Some("10.0.0.1".to_string()),
            "IP should be preserved"
        );
        assert_eq!(
            host.architecture,
            Some("aarch64".to_string()),
            "Architecture should be preserved"
        );
    }

    #[test]
    fn test_get_host_not_found() {
        let store = StateStore::new();
        assert!(
            store.get_host("ff:ff:ff:ff:ff:ff").is_none(),
            "Getting a nonexistent host should return None"
        );
    }

    #[test]
    fn test_list_hosts_returns_all() {
        let store = StateStore::new();

        store.update_host_status(
            "aa:00:00:00:00:01",
            HostStatus::Polling,
            None,
            None,
            None,
            None,
        );
        store.update_host_status(
            "aa:00:00:00:00:02",
            HostStatus::Booting,
            None,
            None,
            None,
            None,
        );
        store.update_host_status(
            "aa:00:00:00:00:03",
            HostStatus::Completed,
            None,
            None,
            None,
            None,
        );

        let hosts = store.list_hosts();
        assert_eq!(
            hosts.len(),
            3,
            "list_hosts should return all 3 inserted hosts"
        );
    }

    #[test]
    fn test_log_event_ordering() {
        let store = StateStore::new();

        store.log_event("INFO", None, "first event");
        store.log_event("WARN", Some("aa:bb:cc:dd:ee:ff"), "second event");
        store.log_event("ERROR", None, "third event");

        let logs = store.list_logs();
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].message, "first event");
        assert_eq!(logs[1].message, "second event");
        assert_eq!(logs[2].message, "third event");

        // Verify levels are preserved
        assert_eq!(logs[0].level, "INFO");
        assert_eq!(logs[1].level, "WARN");
        assert_eq!(logs[2].level, "ERROR");

        // Verify MAC normalization in log events
        assert_eq!(logs[1].mac, Some("aa:bb:cc:dd:ee:ff".to_string()));
    }
}
