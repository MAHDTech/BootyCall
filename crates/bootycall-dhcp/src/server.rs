use crate::error::DhcpError;
use bootycall_core::config::{Config, ServerConfig};
use bootycall_core::state::{HostStatus, StateStore};
use bootycall_log::{debug, error, info, warn};
use dhcproto::{Decodable, Decoder, Encodable, Encoder, v4};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
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

/// BOOTP/DHCP carries `chaddr` in a fixed 16-byte field, so a valid `hlen`
/// can never exceed this. `dhcproto` 0.15 decodes `hlen` from the wire
/// without clamping it while storing `chaddr` as `[u8; 16]`, and its
/// `Message::chaddr()` accessor slices `&self.chaddr[..hlen]` — so any
/// `hlen > 16` panics. We must enforce the bound ourselves (issue 066).
const CHADDR_MAX_LEN: usize = 16;
/// Shortest hardware address we accept: we need at least an Ethernet MAC's
/// worth of bytes to identify the client.
const MAC_LEN: usize = 6;

static IP_RESOLVE_CACHE: OnceLock<RwLock<HashMap<Ipv4Addr, Ipv4Addr>>> = OnceLock::new();

fn get_cached_local_ip(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let cache = IP_RESOLVE_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    cache.read().get(&target).copied()
}

fn cache_local_ip(target: Ipv4Addr, local_ip: Ipv4Addr) {
    let cache = IP_RESOLVE_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    cache.write().insert(target, local_ip);
}

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

async fn resolve_local_ip(request: &v4::Message, socket: &UdpSocket) -> Ipv4Addr {
    let ciaddr = request.ciaddr();
    let giaddr = request.giaddr();
    let local_addr = socket.local_addr().ok();

    // 1. Try checking cache first for direct lookups to avoid spawn_blocking entirely when cached.
    if !ciaddr.is_unspecified() {
        let ip_opt = get_cached_local_ip(ciaddr);
        if let Some(ip) = ip_opt {
            return ip;
        }
    }
    if !giaddr.is_unspecified() {
        let ip_opt = get_cached_local_ip(giaddr);
        if let Some(ip) = ip_opt {
            return ip;
        }
    }

    // If cache miss, or no ciaddr/giaddr, run the resolution logic inside spawn_blocking.
    tokio::task::spawn_blocking(move || {
        // 1. Try resolving using client's IP (ciaddr)
        if !ciaddr.is_unspecified() {
            let ip_opt = get_local_ip_for_target(ciaddr);
            if let Some(ip) = ip_opt {
                cache_local_ip(ciaddr, ip);
                return ip;
            }
        }

        // 2. Try resolving using relay IP (giaddr)
        if !giaddr.is_unspecified() {
            let ip_opt = get_local_ip_for_target(giaddr);
            if let Some(ip) = ip_opt {
                cache_local_ip(giaddr, ip);
                return ip;
            }
        }

        // 3. Fallback: look up the local address of the socket
        if let Some(addr) = local_addr {
            match addr.ip() {
                std::net::IpAddr::V4(ip) if !ip.is_unspecified() => return ip,
                _ => {}
            }
        }

        // 4. Fallback: return default local IP using target routing
        let default_target = Ipv4Addr::new(8, 8, 8, 8);
        let cached_default_opt = get_cached_local_ip(default_target);
        if let Some(cached_ip) = cached_default_opt {
            return cached_ip;
        }
        let default_ip = get_default_local_ip();
        cache_local_ip(default_target, default_ip);
        default_ip
    })
    .await
    .unwrap_or_else(|_| Ipv4Addr::new(127, 0, 0, 1))
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

/// A decoded, validated PXE boot request extracted from a raw DHCP datagram.
///
/// Produced only by [`parse_pxe_request`]; holding one means the datagram
/// decoded cleanly, carried a plausible hardware address length, advertised
/// `PXEClient` (Option 60), and is a message type we answer.
struct PxeRequest {
    /// The decoded DHCP message, used to craft the reply.
    message: v4::Message,
    /// The client MAC (first six `chaddr` bytes), normalised via
    /// [`bootycall_core::format_mac`].
    mac_str: String,
    /// Client system architecture (Option 93), if advertised.
    arch: Option<v4::Architecture>,
    /// The DHCP message type to answer with: Offer for Discover, Ack for
    /// Request/Inform.
    response_type: v4::MessageType,
}

/// Decode and validate one raw datagram into a [`PxeRequest`].
///
/// Returns `None` (after a `debug!` log) for anything the proxy must ignore:
/// undecodable packets, hardware address lengths outside
/// [`MAC_LEN`]`..=`[`CHADDR_MAX_LEN`], non-PXE clients (no `PXEClient` in
/// Option 60, per RFC 4578), and message types other than
/// Discover/Request/Inform.
///
/// Security (issue 066): the `hlen` bounds check runs **before** the first
/// `chaddr()` call. `hlen` is attacker-controlled and `dhcproto` 0.15 decodes
/// it unclamped, so `chaddr()` (which slices `[..hlen]` on a fixed
/// `[u8; 16]`) panics for `hlen > 16`. Without this guard a single crafted
/// UDP datagram unwinds `run_dhcp_server` and takes the whole process down —
/// a remote unauthenticated DoS.
#[tracing::instrument(skip_all, fields(%src_addr, packet_len = packet.len()))]
fn parse_pxe_request(packet: &[u8], src_addr: SocketAddr) -> Option<PxeRequest> {
    let request = match v4::Message::decode(&mut Decoder::new(packet)) {
        Ok(msg) => msg,
        Err(e) => {
            debug!("Failed to decode DHCP packet from {}: {:?}", src_addr, e);
            return None;
        }
    };

    // Reject bogus hardware address lengths before *any* `chaddr()` access:
    // 0 is meaningless, 1..=5 is too short to hold an Ethernet MAC, and
    // anything above 16 overruns dhcproto's fixed array (a panic).
    let hlen = usize::from(request.hlen());
    if !(MAC_LEN..=CHADDR_MAX_LEN).contains(&hlen) {
        debug!(
            "Skipping packet from {} with invalid hardware address length: {}",
            src_addr, hlen
        );
        return None;
    }

    // Safe: `hlen` is validated to `6..=16` above and `chaddr()` returns
    // exactly `hlen` bytes, so the six-byte MAC prefix always exists. Route
    // through the shared core helper rather than a bespoke `format!`
    // (issue 030).
    let mac_bytes = request.chaddr();
    let mac_array: [u8; MAC_LEN] = mac_bytes[..MAC_LEN].try_into().ok()?;
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
        return None;
    }

    // Parse client system architecture (Option 93)
    let arch = match request.opts().get(v4::OptionCode::ClientSystemArchitecture) {
        Some(v4::DhcpOption::ClientSystemArchitecture(arch)) => Some(*arch),
        _ => None,
    };

    // Determine message type and reply message type
    let response_type = match request.opts().msg_type() {
        Some(v4::MessageType::Discover) => v4::MessageType::Offer,
        Some(v4::MessageType::Request) | Some(v4::MessageType::Inform) => v4::MessageType::Ack,
        other => {
            debug!(
                "Ignoring DHCP message type {:?} from {} (mac {})",
                other, src_addr, mac_str
            );
            return None;
        }
    };

    Some(PxeRequest {
        message: request,
        mac_str,
        arch,
        response_type,
    })
}

/// Pick the default bootloader for a client's advertised architecture
/// (Option 93). arch 0 is a legacy real-mode BIOS PXE ROM that cannot execute
/// an EFI image, so it gets `default_bootloader_bios`; arch 11 gets the arm64
/// EFI default; X64/BC EFI — or a client that sent no Option 93 at all — gets
/// the amd64 EFI default. Every other architecture (IA32 EFI, ARM32 EFI, the
/// UEFI HTTP-boot classes, ...) has no image this server can hand out, so
/// selection returns `None` and the caller must refuse the request instead of
/// silently serving an amd64 binary the machine cannot execute. Used only
/// when the host has no explicit `bootloader` override.
fn select_default_bootloader(
    server: &ServerConfig,
    arch: Option<v4::Architecture>,
) -> Option<String> {
    match arch {
        Some(a) if a.0 == ARCH_ARM64 => Some(server.default_bootloader_arm64.clone()),
        Some(a) if a.0 == ARCH_BIOS_X86 => Some(server.default_bootloader_bios.clone()),
        Some(v4::Architecture::X64) | Some(v4::Architecture::BC) | None => {
            Some(server.default_bootloader_amd64.clone())
        }
        Some(_) => None,
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
///
/// Span layout: `#[instrument]` on this function would produce a single span
/// for the whole process lifetime (the loop never returns), so per-request
/// context comes from [`respond_to_pxe_request`] instead — the loop itself
/// carries one long-lived `run_dhcp_server{bind_addr}` span that groups every
/// nested record under the DHCP subsystem.
#[tracing::instrument(skip(config, state_store))]
pub async fn run_dhcp_server(
    bind_addr: &str,
    config: Arc<parking_lot::RwLock<Config>>,
    state_store: StateStore,
) -> Result<(), DhcpError> {
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

        // Decode, validate, and parse in one testable step; anything the
        // proxy must ignore (including the malicious `hlen > 16` packets
        // that used to panic the process, issue 066) yields `None`.
        let Some(parsed) = parse_pxe_request(&buf[..len], src_addr) else {
            continue;
        };
        respond_to_pxe_request(&socket, &config, &state_store, parsed, src_addr).await;
    }
}

/// Answer one validated PXE request: pick the bootloader, update the state
/// store, emit the `dhcp_pxe_offer` event, and send the proxy-DHCP reply.
///
/// This is the per-request unit of work extracted from the `run_dhcp_server`
/// loop so each request gets its own span (client address + MAC); every log
/// and `bootycall_log::event!` record emitted while answering carries that
/// context. `skip_all` because none of the args are cheap/useful to Debug
/// (socket, shared config/state, and the decoded `v4::Message`).
#[tracing::instrument(skip_all, fields(%src_addr, mac = %parsed.mac_str))]
async fn respond_to_pxe_request(
    socket: &UdpSocket,
    config: &parking_lot::RwLock<Config>,
    state_store: &StateStore,
    parsed: PxeRequest,
    src_addr: SocketAddr,
) {
    let PxeRequest {
        message: request,
        mac_str,
        arch,
        response_type,
    } = parsed;

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

    // Read configuration in a nested block to drop the lock guard before await points
    let (bootloader_path, host_name) = {
        let config_guard = config.read();
        let host_config = config_guard.find_host(&mac_str);
        let bootloader_path = match host_config.and_then(|h| h.bootloader.clone()) {
            Some(override_path) => Some(override_path),
            None => select_default_bootloader(&config_guard.server, arch),
        };
        let host_name = host_config.map(|h| h.name.clone());
        (bootloader_path, host_name)
    };

    // No configured image can run on this architecture (e.g. IA32 EFI or
    // ARM32 EFI): refuse loudly and stay silent on the wire instead of
    // serving the amd64 EFI default the machine cannot execute — the
    // client sees no offer and the operator sees why.
    let Some(bootloader_path) = bootloader_path else {
        warn!(
            "Refusing PXE request from {} (mac {}): unsupported client architecture {:?} and no per-host bootloader override",
            src_addr,
            mac_str,
            arch.map(|a| a.0)
        );
        let _ = state_store.log_event(
            "WARN",
            Some(&mac_str),
            &format!(
                "Refused PXE request: unsupported client architecture {:?}",
                arch.map(|a| a.0)
            ),
        );
        return;
    };

    // Resolve Next-Server IP
    let our_ip = resolve_local_ip(&request, socket).await;

    // Update StateStore
    let _ = state_store.update_host_status(
        &mac_str,
        HostStatus::Polling,
        host_name,
        None,
        Some(client_ip_str.clone()),
        Some(arch_str.to_string()),
    );

    let _ = state_store.log_event(
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

    // Craft reply message. `request.chaddr()` (and `new_with_id`'s
    // internal `chaddr.len() <= 16` assert) are safe here only because
    // `parse_pxe_request` already validated `hlen`.
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

    // Copy Client Machine Identifier (Option 97) if present
    if let Some(machine_id) = request.opts().get(v4::OptionCode::ClientMachineIdentifier) {
        reply.opts_mut().insert(machine_id.clone());
    }

    // Encode reply
    let mut response_buf = Vec::new();
    let mut encoder = Encoder::new(&mut response_buf);
    if let Err(e) = reply.encode(&mut encoder) {
        error!("Failed to encode DHCP reply: {:?}", e);
        return;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw DHCPDISCOVER datagram, byte by byte, with an arbitrary
    /// `hlen`. dhcproto's encoder clamps `hlen` on the way out, so crafting
    /// the malicious `hlen > 16` packets of issue 066 requires hand-rolling
    /// the fixed BOOTP layout: op, htype, hlen, hops, xid, secs, flags,
    /// ciaddr, yiaddr, siaddr, giaddr, chaddr[16], sname[64], file[128],
    /// magic cookie, then options. Option 60 carries `PXEClient` and
    /// Option 53 is Discover, so the packet passes every other check and the
    /// result isolates the `hlen` validation under test.
    fn raw_pxe_discover_with_hlen(hlen: u8) -> Vec<u8> {
        let mut p = Vec::with_capacity(300);
        p.push(1); // op: BOOTREQUEST
        p.push(1); // htype: Ethernet
        p.push(hlen); // hlen: attacker-controlled, deliberately unclamped
        p.push(0); // hops
        p.extend_from_slice(&0x1234_5678_u32.to_be_bytes()); // xid
        p.extend_from_slice(&[0u8; 2]); // secs
        p.extend_from_slice(&[0u8; 2]); // flags
        p.extend_from_slice(&[0u8; 16]); // ciaddr, yiaddr, siaddr, giaddr
        // chaddr is always 16 bytes on the wire regardless of hlen.
        p.extend_from_slice(&[
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x10,
        ]);
        p.extend_from_slice(&[0u8; 64]); // sname
        p.extend_from_slice(&[0u8; 128]); // file
        p.extend_from_slice(&[99, 130, 83, 99]); // DHCP magic cookie
        p.extend_from_slice(&[53, 1, 1]); // Option 53: message type Discover
        p.push(60); // Option 60: vendor class identifier
        p.push(9);
        p.extend_from_slice(b"PXEClient");
        p.push(255); // end option
        p
    }

    fn test_src() -> SocketAddr {
        "192.0.2.10:68".parse().unwrap()
    }

    #[test]
    fn oversized_hlen_is_rejected_without_panicking() {
        // Regression for issue 066: any `hlen` in 17..=255 used to panic
        // inside dhcproto's `chaddr()` ("range end index out of range for
        // slice of length 16"), unwinding the whole DHCP task.
        for hlen in [17u8, 255] {
            assert!(
                parse_pxe_request(&raw_pxe_discover_with_hlen(hlen), test_src()).is_none(),
                "hlen {hlen} must be rejected, not parsed (or worse, panic)"
            );
        }
    }

    #[test]
    fn zero_and_undersized_hlen_are_rejected() {
        // hlen 0 is meaningless; 1..=5 cannot hold an Ethernet MAC.
        for hlen in [0u8, 1, 5] {
            assert!(
                parse_pxe_request(&raw_pxe_discover_with_hlen(hlen), test_src()).is_none(),
                "hlen {hlen} must be rejected"
            );
        }
    }

    #[test]
    fn valid_hlen_values_still_parse() {
        // 6 (Ethernet), 12, and 16 (the chaddr field maximum) must all keep
        // working; the fix may only reject 0 and >16 on top of the
        // pre-existing <6 floor.
        for hlen in [6u8, 12, 16] {
            let parsed = parse_pxe_request(&raw_pxe_discover_with_hlen(hlen), test_src())
                .unwrap_or_else(|| panic!("hlen {hlen} must parse successfully"));
            // The MAC is always the first six chaddr bytes.
            assert_eq!(parsed.mac_str, "aa:bb:cc:dd:ee:ff");
            assert_eq!(parsed.response_type, v4::MessageType::Offer);
            assert_eq!(parsed.message.hlen(), hlen);
        }
    }

    #[test]
    fn non_pxe_clients_are_still_filtered_after_the_refactor() {
        // Strip Option 60 ("PXEClient") off an otherwise valid Discover; the
        // RFC 4578 filter must still reject it inside the parse helper.
        let mut packet = raw_pxe_discover_with_hlen(6);
        let opt60_start = packet.len() - 12; // 60, len 9, "PXEClient", 255
        packet.truncate(opt60_start);
        packet.push(255); // restore the end option
        assert!(parse_pxe_request(&packet, test_src()).is_none());
    }

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
            oled_brightness: 255,
            api_token: None,
            max_artifact_bytes: None,
            advertised_host: None,
            allowed_hosts: Vec::new(),
        }
    }

    #[test]
    fn bootloader_selection_routes_arch_to_the_right_default() {
        let server = server_with_bootloaders("amd64.efi", "arm64.efi", "bios.kpxe");

        // arch 0 → BIOS (the regression issue 033 fixed: it used to fall
        // through to the amd64 EFI default, which a BIOS ROM cannot run).
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture(ARCH_BIOS_X86))),
            Some("bios.kpxe".to_string())
        );
        // arch 11 → arm64 EFI.
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture(ARCH_ARM64))),
            Some("arm64.efi".to_string())
        );
        // X64 / BC / absent Option 93 all → amd64 EFI default.
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture::X64)),
            Some("amd64.efi".to_string())
        );
        assert_eq!(
            select_default_bootloader(&server, Some(v4::Architecture::BC)),
            Some("amd64.efi".to_string())
        );
        assert_eq!(
            select_default_bootloader(&server, None),
            Some("amd64.efi".to_string())
        );
    }

    #[test]
    fn unsupported_arches_get_no_bootloader() {
        let server = server_with_bootloaders("amd64.efi", "arm64.efi", "bios.kpxe");

        // IA32 EFI (6), ARM32 EFI (10), the UEFI HTTP-boot classes (15/16),
        // and unassigned codes have no image this server can hand out;
        // selection must refuse (`None`) instead of falling through to the
        // amd64 EFI default those machines cannot execute.
        for arch_code in [6u16, 10, 15, 16, 0xFFFF] {
            assert_eq!(
                select_default_bootloader(&server, Some(v4::Architecture(arch_code))),
                None,
                "arch {arch_code} must be refused, not served an amd64 image"
            );
        }
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

    /// Span-nesting evidence (issue 079): the `dhcp_pxe_offer` event emitted
    /// while answering a request must carry the per-request
    /// `respond_to_pxe_request` span (client address + MAC) in the JSON
    /// output consumed by the downstream log pipeline.
    #[tokio::test]
    async fn dhcp_offer_event_nests_inside_the_per_request_span() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::util::SubscriberInitExt;

        /// A `MakeWriter` that appends everything into a shared buffer so the
        /// test can inspect the formatted JSON output.
        #[derive(Clone, Default)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("buffer lock").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = BufWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(buf.clone())
            .finish();
        // Thread-local default: the current-thread tokio test runtime keeps
        // every poll of `respond_to_pxe_request` on this thread.
        let guard = subscriber.set_default();

        // Loopback source so the reply send stays on-host; the discover has
        // no broadcast flag/relay, so `choose_reply_dest` unicasts to it.
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let src: SocketAddr = "127.0.0.1:16868".parse().unwrap();
        let parsed =
            parse_pxe_request(&raw_pxe_discover_with_hlen(6), src).expect("valid discover");
        let config = parking_lot::RwLock::new(Config {
            server: server_with_bootloaders("amd64.efi", "arm64.efi", "bios.kpxe"),
            hosts: Vec::new(),
        });
        let state_store = StateStore::new();

        respond_to_pxe_request(&socket, &config, &state_store, parsed, src).await;
        drop(guard);

        let bytes = buf.0.lock().expect("buffer lock").clone();
        let out = String::from_utf8(bytes).expect("log output must be UTF-8");
        let offer = out
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("each line is one JSON log")
            })
            .find(|v| v["fields"]["event"] == "dhcp_pxe_offer")
            .expect("a dhcp_pxe_offer event must be emitted");

        // The event's innermost span is the per-request one, carrying the
        // correlating client address and MAC fields.
        assert_eq!(offer["span"]["name"], "respond_to_pxe_request");
        assert_eq!(offer["span"]["mac"], "aa:bb:cc:dd:ee:ff");
        assert_eq!(offer["span"]["src_addr"], "127.0.0.1:16868");
        let spans = offer["spans"].as_array().expect("span list present");
        assert!(
            spans.iter().any(|s| s["name"] == "respond_to_pxe_request"),
            "the span list must include the per-request span, got: {spans:?}"
        );
    }

    #[tokio::test]
    async fn resolve_local_ip_caches_results_and_resolves_correctly() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target_ip = Ipv4Addr::new(127, 0, 0, 1);
        let request = v4::Message::new_with_id(
            123,
            target_ip,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &[0u8; 16],
        );

        // Make sure target_ip is not in cache before starting
        {
            let cache = IP_RESOLVE_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
            cache.write().remove(&target_ip);
        }
        assert!(get_cached_local_ip(target_ip).is_none());

        // Call resolve_local_ip (which will do standard resolution and cache it)
        let resolved_ip = resolve_local_ip(&request, &socket).await;

        // Now, it should be cached!
        let cached_ip = get_cached_local_ip(target_ip).expect("IP must be cached");
        assert_eq!(resolved_ip, cached_ip);

        // Change the resolved cache value to a dummy IP to verify the cache hit works
        let dummy_ip = Ipv4Addr::new(192, 0, 2, 99);
        cache_local_ip(target_ip, dummy_ip);

        // Query again, it should return the cached dummy_ip directly
        let resolved_again = resolve_local_ip(&request, &socket).await;
        assert_eq!(resolved_again, dummy_ip);
    }
}
