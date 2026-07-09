use bootycall_core::config::{Config, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use bootycall_log::{debug, error, info};
use dhcproto::{Decodable, Decoder, Encodable, Encoder, v4};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

/// PXE client system architecture (DHCP Option 93 / RFC 5970) values we route
/// on explicitly. `X64`/`BC` are matched via `dhcproto`'s named variants.
///
/// `ARCH_BIOS_X86` (0) is a legacy real-mode BIOS PXE client: it cannot execute
/// an EFI image, so it needs a dedicated BIOS bootloader. `ARCH_ARM64` (11) is
/// EFI ARM 64-bit.
const ARCH_BIOS_X86: u16 = 0;
const ARCH_ARM64: u16 = 11;

/// The standard DHCP server port. When we are bound here (rather than the PXE
/// proxy port, e.g. 4011) the RFC 2131 relay/ciaddr/broadcast reply routing
/// applies.
const DHCP_SERVER_PORT: u16 = 67;
/// The bootpc (DHCP client) port replies are addressed to when not relayed.
const DHCP_CLIENT_PORT: u16 = 68;
/// The PXE proxy-DHCP port; clients that reached us here listen on it too.
const PXE_PROXY_PORT: u16 = 4011;

fn get_local_ip_for_target(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .connect(SocketAddr::new(std::net::IpAddr::V4(target), 9))
        .ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

fn get_default_local_ip() -> Ipv4Addr {
    get_local_ip_for_target(Ipv4Addr::new(8, 8, 8, 8))
        .unwrap_or_else(|| Ipv4Addr::new(127, 0, 0, 1))
}

fn resolve_local_ip(request: &v4::Message, socket: &UdpSocket) -> Ipv4Addr {
    // 1. Try resolving using client's IP (ciaddr)
    let ciaddr = request.ciaddr();
    if !ciaddr.is_unspecified() {
        let ip_opt = get_local_ip_for_target(ciaddr);
        if let Some(ip) = ip_opt {
            return ip;
        }
    }

    // 2. Try resolving using relay IP (giaddr)
    let giaddr = request.giaddr();
    if !giaddr.is_unspecified() {
        let ip_opt = get_local_ip_for_target(giaddr);
        if let Some(ip) = ip_opt {
            return ip;
        }
    }

    // 3. Fallback: look up the local address of the socket
    if let Ok(addr) = socket.local_addr() {
        match addr.ip() {
            std::net::IpAddr::V4(ip) if !ip.is_unspecified() => return ip,
            _ => {}
        }
    }

    // 4. Fallback: return default local IP using target routing
    get_default_local_ip()
}

/// Returns true iff DHCP Option 60 (Vendor Class Identifier) is present and
/// contains the ASCII substring `PXEClient`. Per RFC 4578 a PXE proxy DHCP
/// server MUST only answer clients advertising this identifier.
fn is_pxe_client(request: &v4::Message) -> bool {
    match request.opts().get(v4::OptionCode::ClassIdentifier) {
        Some(v4::DhcpOption::ClassIdentifier(bytes)) => bytes.windows(9).any(|w| w == b"PXEClient"),
        _ => false,
    }
}

/// Pick the default bootloader for a client's advertised architecture
/// (Option 93). arch 0 is a legacy real-mode BIOS PXE ROM that cannot execute
/// an EFI image, so it gets `default_bootloader_bios`; arch 11 gets the arm64
/// EFI default; everything else (X64/BC EFI, or unknown) gets the amd64 EFI
/// default. Used only when the host has no explicit `bootloader` override.
fn select_default_bootloader(server: &ServerConfig, arch: Option<v4::Architecture>) -> String {
    match arch {
        Some(a) if a.0 == ARCH_ARM64 => server.default_bootloader_arm64.clone(),
        Some(a) if a.0 == ARCH_BIOS_X86 => server.default_bootloader_bios.clone(),
        _ => server.default_bootloader_amd64.clone(),
    }
}

/// Decide where to send the proxy-DHCP reply.
///
/// Ordering follows RFC 2131 §4.1:
/// 1. A client that set the broadcast flag and is **not** behind a relay
///    (`giaddr` unspecified) cannot receive a unicast reply on its
///    still-unconfigured interface — broadcast to `255.255.255.255` on the
///    client port. PXE NICs commonly set this flag, so honouring it is what
///    stops intermittent "reply sent but client never boots" failures. This is
///    checked first so it applies in both proxy (4011) and standard (67) modes.
/// 2. Otherwise the pre-existing unicast behaviour is preserved unchanged: on a
///    non-standard proxy port reply straight back to the source; on port 67
///    relay via `giaddr:67`, else `ciaddr:68`, else the source, else broadcast.
fn choose_reply_dest(
    local_port: u16,
    src_addr: SocketAddr,
    giaddr: Ipv4Addr,
    ciaddr: Ipv4Addr,
    broadcast_flag: bool,
) -> SocketAddr {
    // Clients that reached us on the PXE proxy port listen there; everyone
    // else uses the bootpc port.
    let client_port = if src_addr.port() == PXE_PROXY_PORT {
        PXE_PROXY_PORT
    } else {
        DHCP_CLIENT_PORT
    };

    if broadcast_flag && giaddr.is_unspecified() {
        return SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), client_port);
    }

    if local_port != DHCP_SERVER_PORT {
        return src_addr;
    }
    if !giaddr.is_unspecified() {
        return SocketAddr::new(IpAddr::V4(giaddr), DHCP_SERVER_PORT);
    }
    if !ciaddr.is_unspecified() {
        return SocketAddr::new(IpAddr::V4(ciaddr), DHCP_CLIENT_PORT);
    }
    if !src_addr.ip().is_unspecified() {
        return src_addr;
    }
    SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), client_port)
}

/// Runs the Proxy DHCP server UDP loop, handling configuration-based PXE redirection.
pub async fn run_dhcp_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(bind_addr).await?;
    socket.set_broadcast(true)?;
    info!("Proxy DHCP Server listening on {}", bind_addr);

    let mut buf = [0u8; 1500];
    loop {
        let (len, src_addr) = match socket.recv_from(&mut buf).await {
            Ok(res) => res,
            Err(e) => {
                error!("Failed to receive UDP packet: {:?}", e);
                continue;
            }
        };

        let packet = &buf[..len];
        let request = match v4::Message::decode(&mut Decoder::new(packet)) {
            Ok(msg) => msg,
            Err(e) => {
                debug!("Failed to decode DHCP packet from {}: {:?}", src_addr, e);
                continue;
            }
        };

        // Determine client MAC address and normalize it
        let mac_bytes = request.chaddr();
        if mac_bytes.len() < 6 {
            debug!(
                "Skipping packet with invalid hardware address length: {}",
                mac_bytes.len()
            );
            continue;
        }
        // Guarded above: `mac_bytes.len() >= 6`, so the fixed-size conversion
        // cannot fail. Route through the shared core helper rather than a
        // bespoke `format!` (issue 030).
        let mac_array: [u8; 6] = match mac_bytes[..6].try_into() {
            Ok(arr) => arr,
            Err(_) => continue,
        };
        let mac_str = bootycall_core::format_mac(&mac_array);

        // RFC 4578: only reply to clients advertising `PXEClient` in the
        // vendor-class identifier (Option 60). Injecting PXE options into
        // every DHCP message on the segment would disrupt non-PXE clients
        // and violates the spec.
        if !is_pxe_client(&request) {
            debug!(
                "Skipping non-PXE DHCP message from {} (mac {}): missing/unrecognised Option 60",
                src_addr, mac_str
            );
            continue;
        }

        // Parse client system architecture (Option 93)
        let arch = match request.opts().get(v4::OptionCode::ClientSystemArchitecture) {
            Some(v4::DhcpOption::ClientSystemArchitecture(arch)) => Some(*arch),
            _ => None,
        };

        let arch_str = match arch {
            Some(v4::Architecture::X64) => "x86_64",
            Some(v4::Architecture::BC) => "BC (x86_64)",
            Some(a) if a.0 == ARCH_ARM64 => "aarch64",
            Some(a) if a.0 == ARCH_BIOS_X86 => "x86 (BIOS)",
            Some(other) => {
                debug!("Other architecture detected: {:?}", other.0);
                "other"
            }
            None => "unknown",
        };

        // Resolve client IP (use ciaddr or source IP)
        let client_ip = if !request.ciaddr().is_unspecified() {
            request.ciaddr()
        } else if let SocketAddr::V4(addr) = src_addr {
            *addr.ip()
        } else {
            Ipv4Addr::UNSPECIFIED
        };

        let client_ip_str = client_ip.to_string();

        // Determine message type and reply message type
        let response_type = match request.opts().msg_type() {
            Some(v4::MessageType::Discover) => v4::MessageType::Offer,
            Some(v4::MessageType::Request) | Some(v4::MessageType::Inform) => v4::MessageType::Ack,
            _ => {
                // Ignore other message types
                continue;
            }
        };

        // Read configuration in a nested block to drop the lock guard before await points
        let (bootloader_path, host_name) = {
            let config_guard = config.read();
            let host_config = config_guard.find_host(&mac_str);
            let bootloader_path = match host_config.and_then(|h| h.bootloader.clone()) {
                Some(override_path) => override_path,
                None => select_default_bootloader(&config_guard.server, arch),
            };
            let host_name = host_config.map(|h| h.name.clone());
            (bootloader_path, host_name)
        };

        // Resolve Next-Server IP
        let our_ip = resolve_local_ip(&request, &socket);

        // Update StateStore
        state_store.update_host_status(
            &mac_str,
            HostStatus::Polling,
            host_name,
            None,
            Some(client_ip_str.clone()),
            Some(arch_str.to_string()),
        );

        state_store.log_event(
            "INFO",
            Some(&mac_str),
            &format!(
                "Serving PXE redirection (Bootloader: {}, Next-Server: {}) for {}",
                bootloader_path, our_ip, arch_str
            ),
        );

        bootycall_log::event!(
            "dhcp_pxe_offer",
            mac = %mac_str,
            arch = arch_str,
            client_ip = %client_ip_str,
            bootloader = %bootloader_path,
            next_server = %our_ip,
        );

        // Craft reply message
        let mut reply = v4::Message::new_with_id(
            request.xid(),
            request.ciaddr(),
            Ipv4Addr::UNSPECIFIED,
            our_ip,
            request.giaddr(),
            request.chaddr(),
        );

        reply.set_flags(request.flags());
        reply.set_opcode(v4::Opcode::BootReply);

        // Set mandatory options
        reply
            .opts_mut()
            .insert(v4::DhcpOption::MessageType(response_type));
        reply
            .opts_mut()
            .insert(v4::DhcpOption::ServerIdentifier(our_ip));
        reply
            .opts_mut()
            .insert(v4::DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));
        reply.opts_mut().insert(v4::DhcpOption::TFTPServerName(
            our_ip.to_string().as_bytes().to_vec(),
        ));
        reply.opts_mut().insert(v4::DhcpOption::BootfileName(
            bootloader_path.as_bytes().to_vec(),
        ));

        // Copy Client Identifier if present
        if let Some(client_id) = request.opts().get(v4::OptionCode::ClientIdentifier) {
            reply.opts_mut().insert(client_id.clone());
        }

        // Encode reply
        let mut response_buf = Vec::new();
        let mut encoder = Encoder::new(&mut response_buf);
        if let Err(e) = reply.encode(&mut encoder) {
            error!("Failed to encode DHCP reply: {:?}", e);
            continue;
        }

        // Send reply. RFC 2131 §4.1: honour the client's broadcast flag (PXE
        // NICs that cannot yet receive unicast set it) before falling back to
        // the relay/ciaddr/source unicast routing.
        let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let dest_addr = choose_reply_dest(
            local_port,
            src_addr,
            request.giaddr(),
            request.ciaddr(),
            request.flags().broadcast(),
        );

        debug!("Sending DHCP reply to {}", dest_addr);
        if let Err(e) = socket.send_to(&response_buf, dest_addr).await {
            error!("Failed to send DHCP reply to {}: {:?}", dest_addr, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_bootloaders(amd64: &str, arm64: &str, bios: &str) -> ServerConfig {
        ServerConfig {
            http_bind: "0.0.0.0:8080".to_string(),
            tftp_bind: "0.0.0.0:69".to_string(),
            tftp_root: "./tftpboot".into(),
            proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
            cache_dir: "./cache".into(),
            static_dir: "./static".into(),
            default_bootloader_amd64: amd64.to_string(),
            default_bootloader_arm64: arm64.to_string(),
            default_bootloader_bios: bios.to_string(),
            oled_enabled: false,
            api_token: None,
            max_artifact_bytes: None,
        }
    }

    #[test]
    fn bootloader_selection_routes_arch_to_the_right_default() {
        let server = server_with_bootloaders("amd64.efi", "arm64.efi", "bios.kpxe");

        // arch 0 → BIOS (the regression this issue fixes: it used to fall
        // through to the amd64 EFI default, which a BIOS ROM cannot run).
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture(ARCH_BIOS_X86))),
            "bios.kpxe"
        );
        // arch 11 → arm64 EFI.
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture(ARCH_ARM64))),
            "arm64.efi"
        );
        // X64 / BC / unknown all → amd64 EFI default.
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture::X64)),
            "amd64.efi"
        );
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture::BC)),
            "amd64.efi"
        );
        assert_eq!(select_default_bootloader(&server, None), "amd64.efi");
    }

    #[test]
    fn broadcast_flag_forces_a_broadcast_reply() {
        // A broadcast-flagged request with no relay must be answered by
        // broadcast, regardless of the proxy vs standard listen port.
        let src: SocketAddr = "192.0.2.10:4011".parse().unwrap();
        let dest = choose_reply_dest(
            PXE_PROXY_PORT,
            src,
            Ipv4Addr::UNSPECIFIED, // no relay
            Ipv4Addr::UNSPECIFIED, // no ciaddr
            true,                  // broadcast flag set
        );
        assert_eq!(
            dest,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), PXE_PROXY_PORT),
            "broadcast-flagged proxy client must get a broadcast reply on the proxy port"
        );

        // Same, but the client used the bootpc port → broadcast on 68.
        let src68: SocketAddr = "192.0.2.10:68".parse().unwrap();
        let dest68 = choose_reply_dest(
            DHCP_SERVER_PORT,
            src68,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            true,
        );
        assert_eq!(
            dest68,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DHCP_CLIENT_PORT)
        );
    }

    #[test]
    fn broadcast_flag_yields_to_a_relay() {
        // With the broadcast flag set but a relay present, the reply still goes
        // to the relay (RFC 2131: the relay forwards the final broadcast).
        let src: SocketAddr = "192.0.2.10:67".parse().unwrap();
        let relay = Ipv4Addr::new(192, 0, 2, 1);
        let dest = choose_reply_dest(DHCP_SERVER_PORT, src, relay, Ipv4Addr::UNSPECIFIED, true);
        assert_eq!(dest, SocketAddr::new(IpAddr::V4(relay), DHCP_SERVER_PORT));
    }

    #[test]
    fn unicast_paths_are_unchanged_when_the_flag_is_clear() {
        let src: SocketAddr = "192.0.2.10:4011".parse().unwrap();

        // Proxy port, flag clear → straight back to the source (today's path).
        assert_eq!(
            choose_reply_dest(
                PXE_PROXY_PORT,
                src,
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::new(192, 0, 2, 10),
                false,
            ),
            src
        );

        // Standard port, ciaddr set, flag clear → unicast to ciaddr:68.
        let ciaddr = Ipv4Addr::new(192, 0, 2, 20);
        assert_eq!(
            choose_reply_dest(DHCP_SERVER_PORT, src, Ipv4Addr::UNSPECIFIED, ciaddr, false,),
            SocketAddr::new(IpAddr::V4(ciaddr), DHCP_CLIENT_PORT)
        );
    }
}
