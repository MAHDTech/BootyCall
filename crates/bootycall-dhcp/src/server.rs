use bootycall_core::config::Config;
use bootycall_core::state::{HostStatus, StateStore};
use bootycall_log::{debug, error, info};
use dhcproto::{Decodable, Decoder, Encodable, Encoder, v4};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

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
        let mac_str = format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac_bytes[0], mac_bytes[1], mac_bytes[2], mac_bytes[3], mac_bytes[4], mac_bytes[5]
        );

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
            Some(a) if a.0 == 11 => "aarch64",
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
            let bootloader_path = if let Some(host) = host_config {
                if let Some(ref override_path) = host.bootloader {
                    override_path.clone()
                } else {
                    match arch {
                        Some(a) if a.0 == 11 => {
                            config_guard.server.default_bootloader_arm64.clone()
                        }
                        _ => config_guard.server.default_bootloader_amd64.clone(),
                    }
                }
            } else {
                match arch {
                    Some(a) if a.0 == 11 => config_guard.server.default_bootloader_arm64.clone(),
                    _ => config_guard.server.default_bootloader_amd64.clone(),
                }
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

        // Send reply
        let dest_addr = if socket.local_addr().map(|a| a.port()).unwrap_or(0) != 67 {
            src_addr
        } else if !request.giaddr().is_unspecified() {
            SocketAddr::new(std::net::IpAddr::V4(request.giaddr()), 67)
        } else if !request.ciaddr().is_unspecified() {
            SocketAddr::new(std::net::IpAddr::V4(request.ciaddr()), 68)
        } else if !src_addr.ip().is_unspecified() {
            src_addr
        } else {
            let port = if src_addr.port() == 4011 { 4011 } else { 68 };
            SocketAddr::new(
                std::net::IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                port,
            )
        };

        debug!("Sending DHCP reply to {}", dest_addr);
        if let Err(e) = socket.send_to(&response_buf, dest_addr).await {
            error!("Failed to send DHCP reply to {}: {:?}", dest_addr, e);
        }
    }
}
