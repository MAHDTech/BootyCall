# Security Considerations: TFTP Reflection & Amplification Mitigation

TFTP (Trivial File Transfer Protocol) is a UDP-based protocol (RFC 1350) which, by design, lacks authentication and handshake verification for optionless Read Requests (RRQs). This exposes TFTP servers to reflection and amplification attacks if not properly secured.

---

## 1. The Amplification Exposure

An optionless RRQ skips the OACK (Option Acknowledgement) handshake. In a standard TFTP server, this immediately triggers the transmission of the first DATA block (516 bytes) to the source IP address specified in the request.

If the source IP address is spoofed by an attacker, the server will direct this data packet to a victim. Because UDP is stateless, the attacker can spoof requests easily. Furthermore, if the victim does not acknowledge the block, a standard TFTP server will retransmit the first block up to `MAX_RETRIES` (e.g. 5) times.

### Amplification Factor

A spoofed optionless RRQ is typically only 30–50 bytes in size.
Sending 6 blocks of 516 bytes (1 initial + 5 retries) yields a total of 3096 bytes sent to the victim, representing a **~60× to 100× amplification factor**.

---

## 2. Implemented Mitigation

To address this amplification vulnerability without breaking compatibility with legacy TFTP clients that do not support option negotiation (RFC 2347), `bootycall-tftp` implements a lightweight mitigation:

- **OACK Verification:** For requests with options, the client must first acknowledge the OACK (with ACK 0) before any file data is sent. This proves the client's source IP is responsive and not spoofed.
- **Optionless RRQ Retry Clamping:** For optionless requests where OACK is skipped, the retransmission limit for the first DATA block (before any ACK is received from the client) is clamped to **at most 1 retry** (instead of the standard 5).
- **Restored Retry Budget:** Once the client sends a valid ACK for the first block, we consider the source IP validated and restore the full retry budget (up to 5 retries) for the rest of the transfer.

This limits the maximum amplification for spoofed/unresponsive targets to only 2 packets (1 initial + 1 retry), cutting the amplification potential to less than 20% of its original exposure.

---

## 3. Network Security & Firewall Recommendations

While the application-level mitigation reduces the impact of amplification, it does not prevent reflection entirely. TFTP is intended exclusively for local area network (LAN) provisioning. Because of this, administrators **must** configure appropriate network security boundaries:

1. **Firewall / Network Access Control Lists (ACLs):**
   - Do not expose UDP port 69 (TFTP) to the public Internet or untrusted network segments.
   - Restrict access to the TFTP service to the local management VLAN or provisioning network segment where netbooting clients reside.

2. **Ingress Filtering (BCP 38):**
   - Configure edge switch/router ingress filtering to drop packets with spoofed source IP addresses originating from outside the local network.

3. **Rate Limiting:**
   - Implement rate-limiting rules (e.g., via `iptables` or `nftables`) on the host running `bootycall-rs` to bound incoming UDP port 69 requests per client IP.
