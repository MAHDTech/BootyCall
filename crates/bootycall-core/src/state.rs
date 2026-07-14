use crate::error::CoreError;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

/// Upper bound on host entries kept in the state store. SEC-4: an unbounded
/// map lets a MAC-injection flood balloon memory (the state store is fed by
/// every DHCP/HTTP/TFTP touch). When the map is full, `update_host_status`
/// admits a new MAC by evicting the least-recently-seen entry (LRU), so a
/// sustained flood of continuously-refreshed MACs cannot permanently crowd
/// out hosts that appear later. The periodic cleaner still sweeps entries
/// that go idle, reclaiming headroom once a flood subsides.
pub const MAX_TRACKED_HOSTS: usize = 4096;

/// Upper bound on retained log events. The dashboard shows a rolling window, so
/// once the ring is full the oldest events are evicted in O(1) from the front
/// of the `VecDeque`, bounding memory under a chatty event stream.
pub const MAX_LOGS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostStatus {
    Polling,
    Booting,
    Completed,
    Failed,
}

fn default_instant() -> Instant {
    Instant::now()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostState {
    pub mac: String,
    pub name: Option<String>,
    pub status: HostStatus,
    pub assigned_target: Option<String>,
    pub last_seen: SystemTime,
    #[serde(skip, default = "default_instant")]
    pub last_seen_monotonic: Instant,
    pub client_ip: Option<String>,
    pub architecture: Option<String>,
    pub cache_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub timestamp: SystemTime,
    pub level: String,
    pub mac: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
struct Clock {
    mock: Option<Arc<MockClockState>>,
}

#[derive(Debug)]
struct MockClockState {
    system_time: RwLock<SystemTime>,
    instant: RwLock<Instant>,
}

#[cfg(test)]
impl MockClockState {
    fn set_system_time(&self, time: SystemTime) {
        *self.system_time.write() = time;
    }

    fn set_instant(&self, instant: Instant) {
        *self.instant.write() = instant;
    }
}

impl Clock {
    fn real() -> Self {
        Self { mock: None }
    }

    fn now_system(&self) -> SystemTime {
        if let Some(ref mock) = self.mock {
            *mock.system_time.read()
        } else {
            SystemTime::now()
        }
    }

    fn now_instant(&self) -> Instant {
        if let Some(ref mock) = self.mock {
            *mock.instant.read()
        } else {
            Instant::now()
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    hosts: Arc<RwLock<HashMap<String, HostState>>>,
    logs: Arc<RwLock<VecDeque<LogEvent>>>,
    pub cache_ready_hosts: Arc<RwLock<HashMap<String, bool>>>,
    clock: Clock,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    /// Creates a new, empty instance of `StateStore`.
    pub fn new() -> Self {
        Self {
            hosts: Arc::new(RwLock::new(HashMap::new())),
            logs: Arc::new(RwLock::new(VecDeque::new())),
            cache_ready_hosts: Arc::new(RwLock::new(HashMap::new())),
            clock: Clock::real(),
        }
    }

    #[cfg(test)]
    /// Creates a mocked instance of `StateStore` for unit testing with predictable timestamps.
    fn new_mocked(system_now: SystemTime, instant_now: Instant) -> (Self, Arc<MockClockState>) {
        let state = Arc::new(MockClockState {
            system_time: RwLock::new(system_now),
            instant: RwLock::new(instant_now),
        });
        (
            Self {
                hosts: Arc::new(RwLock::new(HashMap::new())),
                logs: Arc::new(RwLock::new(VecDeque::new())),
                cache_ready_hosts: Arc::new(RwLock::new(HashMap::new())),
                clock: Clock {
                    mock: Some(state.clone()),
                },
            },
            state,
        )
    }

    /// Updates the status and other tracked details of a host identified by its MAC address.
    ///
    /// If the MAC address is invalid, returns a `CoreError::InvalidMac` error.
    /// Enforces the maximum tracked hosts limit by evicting the least-recently-seen host (LRU)
    /// if the map is at its ceiling.
    pub fn update_host_status(
        &self,
        mac: &str,
        status: HostStatus,
        name: Option<String>,
        target: Option<String>,
        ip: Option<String>,
        arch: Option<String>,
    ) -> Result<(), CoreError> {
        let normalized = crate::mac::normalize_mac(mac);
        if !crate::mac::is_valid_mac(&normalized) {
            return Err(CoreError::InvalidMac {
                host: name.unwrap_or_default(),
                mac: normalized,
            });
        }
        let cache_ready = self.get_cache_ready(&normalized);
        let mut hosts = self.hosts.write();
        // SEC-4: if we're at the ceiling and this MAC is new, evict the
        // least-recently-seen entry (LRU) to make room instead of dropping
        // the newcomer. Dropping would let an attacker who keeps their
        // flood MACs warm permanently hide every host that appears later;
        // eviction bounds memory while keeping the store live. The O(n)
        // scan only runs on the full-map new-MAC path (n = 4096).
        if !hosts.contains_key(&normalized) && hosts.len() >= MAX_TRACKED_HOSTS {
            let oldest = hosts
                .values()
                .min_by_key(|state| state.last_seen_monotonic)
                .map(|state| state.mac.clone());
            if let Some(oldest_mac) = oldest {
                hosts.remove(&oldest_mac);
            }
        }
        let entry = hosts
            .entry(normalized.clone())
            .or_insert_with(|| HostState {
                mac: normalized,
                name: None,
                status,
                assigned_target: None,
                last_seen: self.clock.now_system(),
                last_seen_monotonic: self.clock.now_instant(),
                client_ip: None,
                architecture: None,
                cache_ready,
            });

        entry.status = status;
        entry.last_seen = self.clock.now_system();
        entry.last_seen_monotonic = self.clock.now_instant();
        entry.cache_ready = cache_ready;
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
        Ok(())
    }

    /// Retrieves the current state of a host by its MAC address.
    ///
    /// The lookup is case-insensitive and normalizes separator hyphens to colons automatically.
    /// Returns `None` if the host is not tracked in the state store.
    pub fn get_host(&self, mac: &str) -> Option<HostState> {
        let hosts = self.hosts.read();
        let normalized = crate::mac::normalize_mac(mac);
        hosts.get(&normalized).cloned()
    }

    /// Retrieves the current state of a host by its IP address.
    ///
    /// Returns `None` if no host with the matching IP is tracked in the state store.
    pub fn get_host_by_ip(&self, ip: &str) -> Option<HostState> {
        let hosts = self.hosts.read();
        hosts
            .values()
            .find(|h| h.client_ip.as_deref() == Some(ip))
            .cloned()
    }

    /// Lists all hosts currently tracked by the state store.
    pub fn list_hosts(&self) -> Vec<HostState> {
        let hosts = self.hosts.read();
        hosts.values().cloned().collect()
    }

    /// Checks if any tracked hosts have reported activity within the specified maximum age duration.
    pub fn has_recent_activity(&self, max_age: std::time::Duration) -> bool {
        let now = self.clock.now_instant();
        let hosts = self.hosts.read();
        hosts.values().any(|h| {
            now.checked_duration_since(h.last_seen_monotonic)
                .map(|age| age <= max_age)
                .unwrap_or(false)
        })
    }

    /// Logs a system or server event associated with an optional MAC address.
    ///
    /// If the provided MAC address is invalid, returns a `CoreError::InvalidMac` error.
    /// Limits the total log entry count by evicting the oldest entries (FIFO) if `MAX_LOGS` is exceeded.
    pub fn log_event(
        &self,
        level: &str,
        mac: Option<&str>,
        message: &str,
    ) -> Result<(), CoreError> {
        let validated_mac = if let Some(m) = mac {
            let normalized = crate::mac::normalize_mac(m);
            if !crate::mac::is_valid_mac(&normalized) {
                return Err(CoreError::InvalidMac {
                    host: String::new(),
                    mac: normalized,
                });
            }
            Some(normalized)
        } else {
            None
        };
        let mut logs = self.logs.write();
        logs.push_back(LogEvent {
            timestamp: self.clock.now_system(),
            level: level.to_string(),
            mac: validated_mac,
            message: message.to_string(),
        });
        // Evict oldest first (O(1) per pop) until back within the ring cap.
        while logs.len() > MAX_LOGS {
            logs.pop_front();
        }
        Ok(())
    }

    /// Lists all event log entries in chronological order.
    pub fn list_logs(&self) -> Vec<LogEvent> {
        let logs = self.logs.read();
        logs.iter().cloned().collect()
    }

    /// Sweeps and evicts hosts that have not been seen for longer than `max_idle_secs` seconds.
    pub fn clean_stale_hosts(&self, max_idle_secs: u64) {
        let mut hosts = self.hosts.write();
        let now = self.clock.now_instant();
        hosts.retain(|_, state| {
            if let Some(duration) = now.checked_duration_since(state.last_seen_monotonic) {
                duration.as_secs() < max_idle_secs
            } else {
                true
            }
        });
    }

    /// Sets the cache readiness for a host.
    pub fn set_cache_ready(&self, mac: &str, ready: bool) {
        let normalized = crate::mac::normalize_mac(mac);
        self.cache_ready_hosts
            .write()
            .insert(normalized.clone(), ready);
        // Also update existing tracked HostState if it is present
        if let Some(host) = self.hosts.write().get_mut(&normalized) {
            host.cache_ready = ready;
        }
    }

    /// Gets the cache readiness for a host.
    pub fn get_cache_ready(&self, mac: &str) -> bool {
        let normalized = crate::mac::normalize_mac(mac);
        let cache_ready = self.cache_ready_hosts.read();
        cache_ready.get(&normalized).copied().unwrap_or(false)
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
        store
            .update_host_status(
                "AA-BB-CC-11-22-33",
                HostStatus::Polling,
                Some("host1".to_string()),
                Some("target1".to_string()),
                Some("192.168.1.100".to_string()),
                Some("x86_64".to_string()),
            )
            .unwrap();

        // MAC address should be normalized
        let host = store.get_host("aa:bb:cc:11:22:33").unwrap();
        assert_eq!(host.name, Some("host1".to_string()));
        assert_eq!(host.status, HostStatus::Polling);
        assert_eq!(host.assigned_target, Some("target1".to_string()));
        assert_eq!(host.client_ip, Some("192.168.1.100".to_string()));
        assert_eq!(host.architecture, Some("x86_64".to_string()));
        assert!(!host.cache_ready);

        // Test cache_ready settings
        assert!(!store.get_cache_ready("aa:bb:cc:11:22:33"));
        store.set_cache_ready("aa:bb:cc:11:22:33", true);
        assert!(store.get_cache_ready("aa:bb:cc:11:22:33"));
        let host_updated = store.get_host("aa:bb:cc:11:22:33").unwrap();
        assert!(host_updated.cache_ready);

        // Log events
        store
            .log_event("INFO", Some("AA-BB-CC-11-22-33"), "Started polling")
            .unwrap();
        let logs = store.list_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].mac, Some("aa:bb:cc:11:22:33".to_string()));
        assert_eq!(logs[0].message, "Started polling");
    }

    #[test]
    fn test_clean_stale_hosts() {
        let store = StateStore::new();

        store
            .update_host_status(
                "aa:bb:cc:00:00:01",
                HostStatus::Polling,
                Some("stale-host".to_string()),
                None,
                None,
                None,
            )
            .unwrap();

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
        store
            .update_host_status(
                "aa:bb:cc:00:00:02",
                HostStatus::Booting,
                Some("my-host".to_string()),
                Some("target-a".to_string()),
                Some("10.0.0.1".to_string()),
                Some("aarch64".to_string()),
            )
            .unwrap();

        // Second update passes None for optional fields — originals should survive
        store
            .update_host_status(
                "aa:bb:cc:00:00:02",
                HostStatus::Completed,
                None,
                None,
                None,
                None,
            )
            .unwrap();

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
    fn test_get_host_by_ip() {
        let store = StateStore::new();
        assert!(
            store.get_host_by_ip("192.168.1.100").is_none(),
            "Getting host by IP when empty should return None"
        );

        store
            .update_host_status(
                "aa:bb:cc:11:22:33",
                HostStatus::Polling,
                Some("host1".to_string()),
                None,
                Some("192.168.1.100".to_string()),
                None,
            )
            .unwrap();

        let host = store.get_host_by_ip("192.168.1.100").unwrap();
        assert_eq!(host.mac, "aa:bb:cc:11:22:33");

        assert!(
            store.get_host_by_ip("192.168.1.101").is_none(),
            "Getting host by non-matching IP should return None"
        );
    }

    #[test]
    fn test_list_hosts_returns_all() {
        let store = StateStore::new();

        store
            .update_host_status(
                "aa:00:00:00:00:01",
                HostStatus::Polling,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .update_host_status(
                "aa:00:00:00:00:02",
                HostStatus::Booting,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .update_host_status(
                "aa:00:00:00:00:03",
                HostStatus::Completed,
                None,
                None,
                None,
                None,
            )
            .unwrap();

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

        store.log_event("INFO", None, "first event").unwrap();
        store
            .log_event("WARN", Some("aa:bb:cc:dd:ee:ff"), "second event")
            .unwrap();
        store.log_event("ERROR", None, "third event").unwrap();

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

    #[test]
    fn test_max_tracked_hosts_ceiling() {
        let store = StateStore::new();

        // Fill the store to exactly the ceiling with distinct MACs.
        // i in 0..4096 maps to 02:00:00:00:HH:LL (HH in 0x00..0x0f, LL in
        // 0x00..0xff) — all distinct, all valid lower-hex.
        for i in 0..MAX_TRACKED_HOSTS {
            let mac = format!("02:00:00:00:{:02x}:{:02x}", (i >> 8) & 0xff, i & 0xff);
            store
                .update_host_status(&mac, HostStatus::Polling, None, None, None, None)
                .unwrap();
        }
        assert_eq!(
            store.list_hosts().len(),
            MAX_TRACKED_HOSTS,
            "store should hold exactly MAX_TRACKED_HOSTS entries"
        );

        // (a) A brand-new MAC at the ceiling is admitted by evicting the
        // least-recently-seen entry; the map never grows past the ceiling.
        store
            .update_host_status(
                "de:ad:be:ef:00:01",
                HostStatus::Polling,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(
            store.get_host("de:ad:be:ef:00:01").is_some(),
            "a new MAC at the ceiling must be admitted via LRU eviction"
        );
        assert_eq!(
            store.list_hosts().len(),
            MAX_TRACKED_HOSTS,
            "eviction must keep the map at the ceiling, never above it"
        );

        // (b) An already-tracked MAC still updates in place at the ceiling.
        let existing = "de:ad:be:ef:00:01";
        store
            .update_host_status(existing, HostStatus::Completed, None, None, None, None)
            .unwrap();
        assert_eq!(
            store.get_host(existing).unwrap().status,
            HostStatus::Completed,
            "existing entries must keep updating at the ceiling"
        );
        assert_eq!(store.list_hosts().len(), MAX_TRACKED_HOSTS);
    }

    #[test]
    fn test_warm_flood_evicts_lru_to_admit_new_mac() {
        let store = StateStore::new();

        // Fill the store to the ceiling with an attacker's distinct MACs.
        for i in 0..MAX_TRACKED_HOSTS {
            let mac = format!("02:00:00:00:{:02x}:{:02x}", (i >> 8) & 0xff, i & 0xff);
            store
                .update_host_status(&mac, HostStatus::Polling, None, None, None, None)
                .unwrap();
        }
        assert_eq!(store.list_hosts().len(), MAX_TRACKED_HOSTS);

        // Let real time pass, then re-send every flood MAC except one victim,
        // keeping the rest "warm" so clean_stale_hosts would never reclaim a
        // slot. The victim keeps its strictly older last_seen timestamp.
        let victim = "02:00:00:00:04:d2"; // i = 1234
        std::thread::sleep(std::time::Duration::from_millis(50));
        for i in 0..MAX_TRACKED_HOSTS {
            let mac = format!("02:00:00:00:{:02x}:{:02x}", (i >> 8) & 0xff, i & 0xff);
            if mac != victim {
                store
                    .update_host_status(&mac, HostStatus::Polling, None, None, None, None)
                    .unwrap();
            }
        }
        assert_eq!(store.list_hosts().len(), MAX_TRACKED_HOSTS);

        // A legitimate host shows up while the flood keeps the map full: it
        // must be admitted, evicting the least-recently-seen entry.
        store
            .update_host_status(
                "aa:bb:cc:dd:ee:0f",
                HostStatus::Polling,
                None,
                None,
                None,
                None,
            )
            .unwrap();

        assert!(
            store.get_host("aa:bb:cc:dd:ee:0f").is_some(),
            "a new MAC must be admitted even when the map is full of warm entries"
        );
        assert!(
            store.get_host(victim).is_none(),
            "the least-recently-seen entry must be the one evicted"
        );
        assert_eq!(
            store.list_hosts().len(),
            MAX_TRACKED_HOSTS,
            "eviction must swap one entry, keeping the map at the ceiling"
        );
    }

    #[test]
    fn test_monotonic_clock_step_resilience() {
        let system_base = SystemTime::now();
        let instant_base = Instant::now();

        let (store, clock_state) = StateStore::new_mocked(system_base, instant_base);

        // Insert a host
        let mac = "00:11:22:33:44:55";
        store
            .update_host_status(mac, HostStatus::Polling, None, None, None, None)
            .unwrap();

        // 1. Simulate a backward clock step in SystemTime
        // Subtract 1 hour from SystemTime, but advance Instant by only 5 seconds.
        let backward_system = system_base - std::time::Duration::from_secs(3600);
        let forward_instant = instant_base + std::time::Duration::from_secs(5);

        clock_state.set_system_time(backward_system);
        clock_state.set_instant(forward_instant);

        // Host should still be considered active / recently seen because monotonic time only advanced by 5s.
        assert!(store.has_recent_activity(std::time::Duration::from_secs(30)));

        // clean_stale_hosts with 30s TTL should not remove the host.
        store.clean_stale_hosts(30);
        assert!(store.get_host(mac).is_some());

        // 2. Simulate a forward clock step in SystemTime
        // Add 1 hour to SystemTime, but advance Instant by only another 5 seconds (total 10 seconds).
        let forward_system = system_base + std::time::Duration::from_secs(3600);
        let forward_instant_2 = instant_base + std::time::Duration::from_secs(10);

        clock_state.set_system_time(forward_system);
        clock_state.set_instant(forward_instant_2);

        // Host should still be active/recent
        assert!(store.has_recent_activity(std::time::Duration::from_secs(30)));
        store.clean_stale_hosts(30);
        assert!(store.get_host(mac).is_some());

        // 3. Monotonic time actually passes past the TTL (e.g. 40 seconds)
        let forward_instant_3 = instant_base + std::time::Duration::from_secs(45);
        clock_state.set_instant(forward_instant_3);

        // Now it should NOT have recent activity within 30s
        assert!(!store.has_recent_activity(std::time::Duration::from_secs(30)));

        // clean_stale_hosts should evict it
        store.clean_stale_hosts(30);
        assert!(store.get_host(mac).is_none());
    }

    #[test]
    fn test_invalid_mac_rejected() {
        let store = StateStore::new();
        let res = store.update_host_status(
            "invalid-mac-address",
            HostStatus::Polling,
            None,
            None,
            None,
            None,
        );
        assert!(res.is_err());
        assert!(matches!(res, Err(CoreError::InvalidMac { .. })));

        let log_res = store.log_event("INFO", Some("invalid-mac-address"), "test message");
        assert!(log_res.is_err());
        assert!(matches!(log_res, Err(CoreError::InvalidMac { .. })));
    }
}
