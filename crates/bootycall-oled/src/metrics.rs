// cspell:ignore iface Iface IRTT
use sysinfo::{Components, Disks, ProcessesToUpdate, System};

/// Bytes per GiB (1024³) — the divisor turning `sysinfo`'s byte counts into the
/// "G" figures shown on the metrics pages.
const BYTES_PER_GIB: f64 = 1_073_741_824.0;

/// Upper sanity bound (°C) on a thermal-zone reading. sysfs occasionally
/// reports bogus spikes; anything at or above this is discarded.
const THERMAL_SANITY_CEILING_C: f32 = 150.0;

/// Format an uptime in seconds as `"Xd Yh Zm"` (no zero-padding), matching the
/// UPTIME metrics page.
fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    format!("{}d {}h {}m", days, hours, mins)
}

/// Format a used/total byte pair as `"U.UG / T.TG"`, or `"N/A"` when `total`
/// is zero (no such device/mount). Shared by the RAM and DISK pages.
fn format_bytes_pair(used: u64, total: u64) -> String {
    if total == 0 {
        return "N/A".to_string();
    }
    let used_gb = used as f64 / BYTES_PER_GIB;
    let total_gb = total as f64 / BYTES_PER_GIB;
    format!("{:.1}G / {:.1}G", used_gb, total_gb)
}

/// Parse a Linux `/sys/class/thermal/*/temp` millidegree reading into degrees
/// Celsius, rejecting values that fail to parse or exceed the sanity ceiling.
/// The trailing newline the sysfs node carries is trimmed.
fn parse_thermal_millidegrees(raw: &str) -> Option<f32> {
    let celsius = raw.trim().parse::<f32>().ok()? / 1000.0;
    (celsius < THERMAL_SANITY_CEILING_C).then_some(celsius)
}

/// True for an interactive `sshd` login process — the appliance's "someone is
/// logged in" signal. Matches the process name `sshd` with a `@pts`
/// pseudo-terminal argument, ignoring the listener daemon and `@notty`
/// sessions.
fn is_interactive_sshd(name: &str, cmd: &[String]) -> bool {
    name == "sshd" && cmd.iter().any(|c| c.contains("@pts"))
}

pub struct SystemMetrics {
    sys: System,
    components: Components,
    disks: Disks,
    thermal_zones: Vec<std::path::PathBuf>,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMetrics {
    pub fn new() -> Self {
        let mut thermal_zones = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/class/thermal") {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.starts_with("thermal_zone"))
                {
                    thermal_zones.push(path);
                }
            }
        }
        Self {
            sys: System::new_all(),
            components: Components::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            thermal_zones,
        }
    }

    pub fn refresh_cpu(&mut self) {
        self.sys.refresh_cpu_all();
    }

    pub fn refresh_memory(&mut self) {
        self.sys.refresh_memory();
    }

    pub fn refresh_components(&mut self) {
        self.components.refresh(true);
    }

    pub fn refresh_disks(&mut self) {
        self.disks.refresh(true);
    }

    pub fn refresh_processes(&mut self) {
        self.sys.refresh_processes(ProcessesToUpdate::All, true);
    }

    pub fn get_hostname(&self) -> String {
        System::host_name()
            .unwrap_or_else(|| "UNKNOWN".to_string())
            .to_uppercase()
    }

    pub fn get_ip_address(&self) -> String {
        if let Some(gateway) = get_default_gateway()
            && let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
                .ok()
                .and_then(|s| {
                    s.connect((gateway, 80)).ok()?;
                    s.local_addr().ok()
                })
                .map(|addr| addr.ip().to_string())
        {
            return truncate_ip(&ip);
        }

        if let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
            .ok()
            .and_then(|s| {
                s.connect("1.1.1.1:80").ok()?;
                s.local_addr().ok()
            })
            .map(|addr| addr.ip().to_string())
        {
            return truncate_ip(&ip);
        }

        if let Some(gateway) = get_default_ipv6_gateway()
            && let Some(ip) = std::net::UdpSocket::bind("[::]:0")
                .ok()
                .and_then(|s| {
                    s.connect((gateway, 80)).ok()?;
                    s.local_addr().ok()
                })
                .map(|addr| addr.ip().to_string())
        {
            return truncate_ip(&ip);
        }

        if let Some(ip) = std::net::UdpSocket::bind("[::]:0")
            .ok()
            .and_then(|s| {
                s.connect((
                    std::net::Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111),
                    80,
                ))
                .ok()?;
                s.local_addr().ok()
            })
            .map(|addr| addr.ip().to_string())
        {
            return truncate_ip(&ip);
        }

        if let Some(ip) = get_local_ips_from_fib_trie()
            .first()
            .map(|ip| ip.to_string())
        {
            return truncate_ip(&ip);
        }

        if let Some(ip) = get_local_ipv6_addresses().first().map(|ip| ip.to_string()) {
            return truncate_ip(&ip);
        }

        "No IP".to_string()
    }

    pub fn get_uptime(&self) -> String {
        format_uptime(System::uptime())
    }

    pub fn get_cpu_temp(&self) -> String {
        let mut max_temp: Option<f32> = None;
        for component in &self.components {
            if let Some(temperature) = component.temperature() {
                max_temp = Some(match max_temp {
                    Some(curr) => curr.max(temperature),
                    None => temperature,
                });
            }
        }
        if max_temp.is_none() {
            for path in &self.thermal_zones {
                let temp_file = path.join("temp");
                if let Ok(content) = std::fs::read_to_string(temp_file)
                    && let Some(temp_c) = parse_thermal_millidegrees(&content)
                {
                    max_temp = Some(match max_temp {
                        Some(curr) => curr.max(temp_c),
                        None => temp_c,
                    });
                }
            }
        }
        if let Some(temp) = max_temp {
            format!("{:.1}C", temp)
        } else {
            "N/A".to_string()
        }
    }

    pub fn get_cpu_usage(&self) -> String {
        let global_cpu = self.sys.global_cpu_usage();
        format!("{:.1}%", global_cpu)
    }

    pub fn get_ram_usage(&self) -> String {
        format_bytes_pair(self.sys.used_memory(), self.sys.total_memory())
    }

    pub fn get_disk_usage(&self) -> String {
        let mut total = 0;
        let mut used = 0;
        for disk in &self.disks {
            if disk.mount_point().to_string_lossy() == "/" {
                total = disk.total_space();
                used = disk.total_space().saturating_sub(disk.available_space());
                break;
            }
        }
        format_bytes_pair(used, total)
    }

    pub fn get_kernel(&self) -> String {
        System::kernel_version().unwrap_or_else(|| "N/A".to_string())
    }

    pub fn active_ssh_sessions(&self) -> usize {
        let mut count = 0;
        for process in self.sys.processes().values() {
            let name = process.name().to_string_lossy();
            let cmd: Vec<String> = process
                .cmd()
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            if is_interactive_sshd(&name, &cmd) {
                count += 1;
            }
        }
        count
    }
}

fn parse_default_gateway(content: &str) -> Option<std::net::Ipv4Addr> {
    for line in content.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(_iface) = parts.next() else {
            continue;
        };
        let Some(dest_hex) = parts.next() else {
            continue;
        };
        let Some(gateway_hex) = parts.next() else {
            continue;
        };

        if dest_hex == "00000000" {
            let Ok(gateway_u32) = u32::from_str_radix(gateway_hex, 16) else {
                continue;
            };
            if gateway_u32 != 0 {
                return Some(std::net::Ipv4Addr::from(gateway_u32.to_ne_bytes()));
            }
        }
    }
    None
}

fn get_default_gateway() -> Option<std::net::Ipv4Addr> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_default_gateway(&content)
}

fn parse_local_ips_from_fib_trie(content: &str) -> Vec<std::net::Ipv4Addr> {
    let mut ips = Vec::new();
    let mut last_ip = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("|--")
            && let Some(ip_str) = trimmed.split_whitespace().last()
            && let Ok(ip) = ip_str.parse::<std::net::Ipv4Addr>()
        {
            last_ip = Some(ip);
        } else if trimmed.contains("host LOCAL")
            && let Some(ip) = last_ip
            && !ip.is_loopback()
        {
            ips.push(ip);
        }
    }
    ips
}

fn get_local_ips_from_fib_trie() -> Vec<std::net::Ipv4Addr> {
    if let Ok(content) = std::fs::read_to_string("/proc/net/fib_trie") {
        parse_local_ips_from_fib_trie(&content)
    } else {
        Vec::new()
    }
}

fn truncate_ip(ip: &str) -> String {
    if ip.len() > 15 {
        let prefix = &ip[..9];
        let suffix = &ip[ip.len() - 4..];
        format!("{}..{}", prefix, suffix)
    } else {
        ip.to_string()
    }
}

fn parse_hex_ipv6(hex_str: &str) -> Option<std::net::Ipv6Addr> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex_str[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(std::net::Ipv6Addr::from(bytes))
}

fn parse_default_ipv6_gateway(content: &str) -> Option<std::net::Ipv6Addr> {
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 {
            continue;
        }
        let dest_hex = parts[0];
        let dest_prefix_hex = parts[1];
        let next_hop_hex = parts[4];

        if dest_hex == "00000000000000000000000000000000"
            && (dest_prefix_hex == "00" || dest_prefix_hex == "0")
            && next_hop_hex != "00000000000000000000000000000000"
            && let Some(next_hop) = parse_hex_ipv6(next_hop_hex)
        {
            return Some(next_hop);
        }
    }
    None
}

fn get_default_ipv6_gateway() -> Option<std::net::Ipv6Addr> {
    let content = std::fs::read_to_string("/proc/net/ipv6_route").ok()?;
    parse_default_ipv6_gateway(&content)
}

fn parse_local_ipv6_addresses(content: &str) -> Vec<std::net::Ipv6Addr> {
    let mut ips = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let ip_hex = parts[0];
        let scope_hex = parts[3];
        let dev_name = parts[5];

        if dev_name == "lo" {
            continue;
        }

        let Some(ip) = parse_hex_ipv6(ip_hex) else {
            continue;
        };

        if ip.is_loopback() {
            continue;
        }

        let Ok(scope) = u8::from_str_radix(scope_hex, 16) else {
            continue;
        };

        if scope == 0x10 {
            continue;
        }

        ips.push((ip, scope));
    }

    ips.sort_by_key(|&(_, scope)| match scope {
        0x00 => 0,
        0x40 => 1,
        0x20 => 2,
        _ => 3,
    });

    ips.into_iter().map(|(ip, _)| ip).collect()
}

fn get_local_ipv6_addresses() -> Vec<std::net::Ipv6Addr> {
    if let Ok(content) = std::fs::read_to_string("/proc/net/if_inet6") {
        parse_local_ipv6_addresses(&content)
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_default_gateway() {
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
enp12s0\t00000000\tFE010A0A\t0003\t0\t0\t1024\t00000000\t0\t0\t0
enp12s0\t00010A0A\t00000000\t0001\t0\t0\t1024\t00FFFFFF\t0\t0\t0
";
        let gateway = parse_default_gateway(route_content);
        assert_eq!(gateway, Some(std::net::Ipv4Addr::new(10, 10, 1, 254)));
    }

    #[test]
    fn test_parse_default_gateway_robustness() {
        let route_content = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT

enp12s0
enp12s0\t00000000
enp12s0\t00000000\tZZZZZZZZ\t0003\t0\t0\t1024\t00000000\t0\t0\t0
enp12s0\t00000000\tFE010A0A\t0003\t0\t0\t1024\t00000000\t0\t0\t0
";
        let gateway = parse_default_gateway(route_content);
        assert_eq!(gateway, Some(std::net::Ipv4Addr::new(10, 10, 1, 254)));
    }

    #[test]
    fn test_parse_local_ips_from_fib_trie() {
        let trie_content = "\
Local:
  +-- 0.0.0.0/0 3 0 5
     +-- 10.10.1.128/25 2 0 2
        |-- 10.10.1.139
           /32 host LOCAL
        |-- 10.10.1.255
           /32 link BROADCAST
     +-- 127.0.0.0/8 2 0 2
        +-- 127.0.0.0/31 1 0 0
           |-- 127.0.0.0
              /8 host LOCAL
           |-- 127.0.0.1
              /32 host LOCAL
";
        let ips = parse_local_ips_from_fib_trie(trie_content);
        assert_eq!(ips, vec![std::net::Ipv4Addr::new(10, 10, 1, 139)]);
    }

    #[test]
    fn test_parse_hex_ipv6() {
        assert_eq!(
            parse_hex_ipv6("00000000000000000000000000000001"),
            Some(std::net::Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1))
        );
        assert_eq!(
            parse_hex_ipv6("fe800000000000000250b6fffe030807"),
            Some(std::net::Ipv6Addr::new(
                0xfe80, 0, 0, 0, 0x0250, 0xb6ff, 0xfe03, 0x0807
            ))
        );
        assert_eq!(parse_hex_ipv6("invalid"), None);
        assert_eq!(parse_hex_ipv6("0000000000000000000000000000000g"), None);
    }

    #[test]
    fn test_parse_default_ipv6_gateway() {
        let route_content = "\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000250b6fffe030807 00000400 00000001 00000000 00000807 enp12s0
20010db8000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000400 00000001 00000000 00000807 enp12s0
";
        assert_eq!(
            parse_default_ipv6_gateway(route_content),
            Some(std::net::Ipv6Addr::new(
                0xfe80, 0, 0, 0, 0x0250, 0xb6ff, 0xfe03, 0x0807
            ))
        );

        let no_route = "\
20010db8000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000400 00000001 00000000 00000807 enp12s0
";
        assert_eq!(parse_default_ipv6_gateway(no_route), None);
    }

    #[test]
    fn test_parse_local_ipv6_addresses() {
        let if_inet6_content = "\
00000000000000000000000000000001 01 80 10 80 lo
fe800000000000000250b6fffe030807 02 40 20 80 eth0
20010db8000000000000000000000001 02 40 00 80 eth0
";
        let ips = parse_local_ipv6_addresses(if_inet6_content);
        assert_eq!(
            ips,
            vec![
                std::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
                std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0x0250, 0xb6ff, 0xfe03, 0x0807),
            ]
        );
    }

    #[test]
    fn test_truncate_ip() {
        assert_eq!(truncate_ip("192.168.1.1"), "192.168.1.1");
        assert_eq!(truncate_ip("2001:db8::1234:5678"), "2001:db8:..5678");
        assert_eq!(
            truncate_ip("2001:db8:3333:4444:5555:6666:7777:8888"),
            "2001:db8:..8888"
        );
    }

    const GIB: u64 = 1_073_741_824;

    #[test]
    fn format_uptime_has_no_zero_padding() {
        assert_eq!(format_uptime(0), "0d 0h 0m");
        assert_eq!(format_uptime(90), "0d 0h 1m");
        assert_eq!(format_uptime(86_400), "1d 0h 0m");
        assert_eq!(
            format_uptime(3 * 86_400 + 14 * 3_600 + 22 * 60),
            "3d 14h 22m"
        );
    }

    #[test]
    fn format_bytes_pair_gib_and_na() {
        assert_eq!(format_bytes_pair(0, 0), "N/A");
        assert_eq!(format_bytes_pair(123, 0), "N/A");
        assert_eq!(format_bytes_pair(4 * GIB, 8 * GIB), "4.0G / 8.0G");
        // 1.5 GiB used, one-decimal rounding.
        assert_eq!(format_bytes_pair(3 * GIB / 2, 8 * GIB), "1.5G / 8.0G");
    }

    #[test]
    fn parse_thermal_trims_scales_and_clamps() {
        assert!(parse_thermal_millidegrees("48500").is_some_and(|v| (v - 48.5).abs() < 1e-3));
        assert!(parse_thermal_millidegrees("48500\n").is_some_and(|v| (v - 48.5).abs() < 1e-3));
        assert_eq!(parse_thermal_millidegrees("200000"), None); // 200°C > ceiling
        assert_eq!(parse_thermal_millidegrees("garbage"), None);
        assert_eq!(parse_thermal_millidegrees(""), None);
    }

    #[test]
    fn interactive_sshd_needs_name_and_pts() {
        assert!(is_interactive_sshd(
            "sshd",
            &["sshd: user@pts/0".to_string()]
        ));
        assert!(!is_interactive_sshd(
            "sshd",
            &["sshd: user@notty".to_string()]
        ));
        assert!(!is_interactive_sshd("bash", &["bash @pts/1".to_string()]));
        assert!(!is_interactive_sshd("sshd", &[]));
    }

    #[test]
    fn test_cpu_temp_handling() {
        use std::fs;
        let temp_dir = tempfile::tempdir().unwrap();

        // 1. All sensor lookups fail / return None -> should return "N/A"
        let metrics_na = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            thermal_zones: vec![],
        };
        assert_eq!(metrics_na.get_cpu_temp(), "N/A");

        // 2. Test negative temperature (-2.5 C -> -2500 millidegrees)
        let zone0 = temp_dir.path().join("thermal_zone0");
        fs::create_dir(&zone0).unwrap();
        fs::write(zone0.join("temp"), "-2500\n").unwrap();

        let metrics_neg = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            thermal_zones: vec![zone0.clone()],
        };
        assert_eq!(metrics_neg.get_cpu_temp(), "-2.5C");

        // 3. Test exactly zero temperature (0.0 C -> 0 millidegrees)
        fs::write(zone0.join("temp"), "0\n").unwrap();
        let metrics_zero = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            thermal_zones: vec![zone0.clone()],
        };
        assert_eq!(metrics_zero.get_cpu_temp(), "0.0C");

        // 4. Test positive temperature (36.7 C -> 36700 millidegrees)
        fs::write(zone0.join("temp"), "36700\n").unwrap();
        let metrics_pos = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            thermal_zones: vec![zone0.clone()],
        };
        assert_eq!(metrics_pos.get_cpu_temp(), "36.7C");

        // 5. Test multiple thermal zones, ensuring we get the max temperature
        let zone1 = temp_dir.path().join("thermal_zone1");
        fs::create_dir(&zone1).unwrap();
        // zone0 has 36.7C, let's write 42.1C to zone1
        fs::write(zone1.join("temp"), "42100\n").unwrap();
        let metrics_multi = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            thermal_zones: vec![zone0, zone1],
        };
        assert_eq!(metrics_multi.get_cpu_temp(), "42.1C");

        // 6. Test fallback when one zone is garbage/bogus
        let zone2 = temp_dir.path().join("thermal_zone2");
        fs::create_dir(&zone2).unwrap();
        fs::write(zone2.join("temp"), "garbage\n").unwrap();

        let zone3 = temp_dir.path().join("thermal_zone3");
        fs::create_dir(&zone3).unwrap();
        fs::write(zone3.join("temp"), "150000\n").unwrap(); // 150C is sanity ceiling, rejected by parse_thermal_millidegrees

        let metrics_bogus = SystemMetrics {
            sys: sysinfo::System::new(),
            components: sysinfo::Components::new(),
            disks: sysinfo::Disks::new(),
            // zone2 has garbage, zone3 has 150C (invalid), so if we only have these, it should return "N/A"
            thermal_zones: vec![zone2, zone3],
        };
        assert_eq!(metrics_bogus.get_cpu_temp(), "N/A");
    }
}
