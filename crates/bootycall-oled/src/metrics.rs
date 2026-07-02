use sysinfo::{Components, Disks, System};

pub struct SystemMetrics {
    sys: System,
    components: Components,
    disks: Disks,
}

impl Default for SystemMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMetrics {
    pub fn new() -> Self {
        Self {
            sys: System::new_all(),
            components: Components::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
        }
    }

    pub fn refresh(&mut self) {
        self.sys.refresh_all();
        self.components.refresh(true);
        self.disks.refresh(true);
    }

    pub fn get_hostname(&self) -> String {
        System::host_name()
            .unwrap_or_else(|| "UNKNOWN".to_string())
            .to_uppercase()
    }

    pub fn get_ip_address(&self) -> String {
        // Quick UDP probe
        if let Some(ip) = std::net::UdpSocket::bind("0.0.0.0:0")
            .ok()
            .and_then(|s| {
                s.connect("1.1.1.1:80").ok()?;
                s.local_addr().ok()
            })
            .map(|addr| addr.ip().to_string())
        {
            return ip;
        }
        "No IP".to_string()
    }

    pub fn get_uptime(&self) -> String {
        let uptime = System::uptime();
        let days = uptime / 86400;
        let hours = (uptime % 86400) / 3600;
        let mins = (uptime % 3600) / 60;
        format!("{}d {}h {}m", days, hours, mins)
    }

    pub fn get_cpu_temp(&self) -> String {
        let mut max_temp = 0.0;
        for component in &self.components {
            if component.temperature().unwrap_or(0.0) > max_temp {
                max_temp = component.temperature().unwrap_or(0.0);
            }
        }
        if max_temp > 0.0 {
            format!("{:.1}C", max_temp)
        } else {
            "N/A".to_string()
        }
    }

    pub fn get_cpu_usage(&self) -> String {
        let global_cpu = self.sys.global_cpu_usage();
        format!("{:.1}%", global_cpu)
    }

    pub fn get_ram_usage(&self) -> String {
        let total = self.sys.total_memory() as f64 / 1_073_741_824.0;
        let used = self.sys.used_memory() as f64 / 1_073_741_824.0;
        format!("{:.1}G / {:.1}G", used, total)
    }

    pub fn get_disk_usage(&self) -> String {
        let mut total = 0;
        let mut used = 0;
        for disk in &self.disks {
            if disk.mount_point().to_string_lossy() == "/" {
                total = disk.total_space();
                used = disk.total_space() - disk.available_space();
                break;
            }
        }
        if total > 0 {
            let total_gb = total as f64 / 1_073_741_824.0;
            let used_gb = used as f64 / 1_073_741_824.0;
            format!("{:.1}G / {:.1}G", used_gb, total_gb)
        } else {
            "N/A".to_string()
        }
    }

    pub fn get_kernel(&self) -> String {
        System::kernel_version().unwrap_or_else(|| "N/A".to_string())
    }

    pub fn active_ssh_sessions(&self) -> usize {
        let mut count = 0;
        for process in self.sys.processes().values() {
            if process.name() == "sshd"
                && process
                    .cmd()
                    .iter()
                    .any(|c| c.to_string_lossy().contains("@pts"))
            {
                count += 1;
            }
        }
        count
    }
}
