//! TFTP wire-format constants and packet encode/decode helpers
//! (RFC 1350, plus RFC 2347 option acknowledgement).
//!
//! Living in one module means the server and the integration tests share a
//! single definition of the packet layout instead of keeping divergent copies.

/// TFTP opcodes (RFC 1350 §5; OACK from RFC 2347).
pub const OP_RRQ: u16 = 1;
pub const OP_WRQ: u16 = 2;
pub const OP_DATA: u16 = 3;
pub const OP_ACK: u16 = 4;
pub const OP_ERROR: u16 = 5;
pub const OP_OACK: u16 = 6;

/// Read the 16-bit big-endian opcode at the front of a packet, if present.
pub fn opcode(pkt: &[u8]) -> Option<u16> {
    if pkt.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([pkt[0], pkt[1]]))
}

/// Encode an ERROR packet (opcode 5): error code + NUL-terminated message.
pub fn make_error_packet(code: u16, msg: &str) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(5 + msg.len());
    pkt.extend_from_slice(&OP_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg.as_bytes());
    pkt.push(0);
    pkt
}

/// Encode an OACK packet (opcode 6): a run of NUL-terminated name/value pairs.
pub fn make_oack_packet(options: &[(&str, String)]) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&OP_OACK.to_be_bytes());
    for (name, val) in options {
        pkt.extend_from_slice(name.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(val.as_bytes());
        pkt.push(0);
    }
    pkt
}

/// Encode a DATA packet (opcode 3): block number + payload.
pub fn make_data_packet(block_num: u16, data: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4 + data.len());
    pkt.extend_from_slice(&OP_DATA.to_be_bytes());
    pkt.extend_from_slice(&block_num.to_be_bytes());
    pkt.extend_from_slice(data);
    pkt
}

/// Encode an ACK packet (opcode 4): block number.
pub fn make_ack_packet(block: u16) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(4);
    pkt.extend_from_slice(&OP_ACK.to_be_bytes());
    pkt.extend_from_slice(&block.to_be_bytes());
    pkt
}

/// Encode an RRQ packet (opcode 1): filename, mode, then option pairs. Used by
/// the integration tests to drive the server.
pub fn make_rrq_packet(filename: &str, mode: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut pkt = Vec::new();
    pkt.extend_from_slice(&OP_RRQ.to_be_bytes());
    pkt.extend_from_slice(filename.as_bytes());
    pkt.push(0);
    pkt.extend_from_slice(mode.as_bytes());
    pkt.push(0);
    for (k, v) in options {
        pkt.extend_from_slice(k.as_bytes());
        pkt.push(0);
        pkt.extend_from_slice(v.as_bytes());
        pkt.push(0);
    }
    pkt
}

/// Parse an ACK packet, returning the acknowledged block number.
pub fn parse_ack_packet(pkt: &[u8]) -> Option<u16> {
    if opcode(pkt)? != OP_ACK || pkt.len() < 4 {
        return None;
    }
    Some(u16::from_be_bytes([pkt[2], pkt[3]]))
}

/// True when the packet is a TFTP ERROR (opcode 5).
pub fn is_error_packet(pkt: &[u8]) -> bool {
    opcode(pkt) == Some(OP_ERROR)
}

/// Parse an ERROR packet into `(code, message)`.
pub fn parse_error_packet(pkt: &[u8]) -> Option<(u16, String)> {
    if opcode(pkt)? != OP_ERROR || pkt.len() < 4 {
        return None;
    }
    let code = u16::from_be_bytes([pkt[2], pkt[3]]);
    let end = pkt[4..]
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(pkt.len() - 4);
    let msg = String::from_utf8_lossy(&pkt[4..4 + end]).into_owned();
    Some((code, msg))
}

/// Parse a DATA packet into `(block_number, payload)`.
pub fn parse_data_packet(pkt: &[u8]) -> Option<(u16, Vec<u8>)> {
    if opcode(pkt)? != OP_DATA || pkt.len() < 4 {
        return None;
    }
    let block = u16::from_be_bytes([pkt[2], pkt[3]]);
    Some((block, pkt[4..].to_vec()))
}

/// Parse an OACK packet into its option name/value pairs.
pub fn parse_oack_packet(pkt: &[u8]) -> Option<Vec<(String, String)>> {
    if opcode(pkt)? != OP_OACK {
        return None;
    }
    let mut parts = Vec::new();
    let mut current = Vec::new();
    for &b in &pkt[2..] {
        if b == 0 {
            parts.push(String::from_utf8_lossy(&current).into_owned());
            current.clear();
        } else {
            current.push(b);
        }
    }
    let mut options = Vec::new();
    let mut i = 0;
    while i + 1 < parts.len() {
        options.push((parts[i].clone(), parts[i + 1].clone()));
        i += 2;
    }
    Some(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_round_trip() {
        let pkt = make_error_packet(4, "nope");
        assert!(is_error_packet(&pkt));
        assert_eq!(parse_error_packet(&pkt), Some((4, "nope".to_string())));
    }

    #[test]
    fn data_and_ack_round_trip() {
        let pkt = make_data_packet(7, b"payload");
        assert_eq!(parse_data_packet(&pkt), Some((7, b"payload".to_vec())));
        assert_eq!(parse_ack_packet(&make_ack_packet(7)), Some(7));
    }

    #[test]
    fn oack_round_trip() {
        let pkt = make_oack_packet(&[("blksize", "1432".to_string())]);
        assert_eq!(
            parse_oack_packet(&pkt),
            Some(vec![("blksize".to_string(), "1432".to_string())])
        );
    }

    #[test]
    fn wrong_opcode_parses_to_none() {
        let rrq = make_rrq_packet("x", "octet", &[]);
        assert_eq!(parse_ack_packet(&rrq), None);
        assert_eq!(parse_data_packet(&rrq), None);
        assert!(!is_error_packet(&rrq));
    }
}
