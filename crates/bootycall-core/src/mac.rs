//! MAC address helpers shared across the config, state store, and HTTP API.
//!
//! Everything in the fleet normalises MAC addresses to lower-hex with colons
//! (`aa:bb:cc:dd:ee:ff`). Before this module the same
//! `to_ascii_lowercase().replace('-', ":")` incantation was repeated in
//! seven places; that duplication meant "close but not identical" behaviour
//! whenever one caller drifted, and it made the SEC-5 MAC validator have
//! nowhere obvious to live.

/// Lowercase the input and swap `-` separators for `:`, matching how the
/// config loader normalises MACs on ingest. Whitespace is trimmed at the
/// edges so callers don't need to pre-clean HTTP path segments.
pub fn normalize_mac(mac: &str) -> String {
    mac.trim().to_ascii_lowercase().replace('-', ":")
}

/// Validate the normalised form: six colon-separated pairs of lower-hex
/// digits, exactly 17 characters, nothing else. This is what the HTTP
/// handlers use to reject junk paths before it reaches the state store.
///
/// The check runs on the *already normalised* string — callers that started
/// with an HTTP `mac:hexhyp` path should call [`normalize_mac`] first.
pub fn is_valid_mac(mac: &str) -> bool {
    if mac.len() != 17 {
        return false;
    }
    for (i, byte) in mac.bytes().enumerate() {
        match i % 3 {
            0 | 1 => {
                if !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase() {
                    return false;
                }
            }
            2 => {
                if byte != b':' {
                    return false;
                }
            }
            _ => unreachable!(),
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_uppercase_and_hyphens() {
        assert_eq!(normalize_mac("AA-BB-CC-11-22-33"), "aa:bb:cc:11:22:33");
    }

    #[test]
    fn normalizes_trims_whitespace() {
        assert_eq!(normalize_mac("  aa:bb:cc:dd:ee:ff  "), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn passes_valid_lower_hex_colons() {
        assert!(is_valid_mac("aa:bb:cc:dd:ee:ff"));
        assert!(is_valid_mac("00:11:22:33:44:55"));
    }

    #[test]
    fn rejects_uppercase_hex() {
        // We only accept the normalised form; callers should have called
        // normalize_mac() first.
        assert!(!is_valid_mac("AA:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn rejects_wrong_separator() {
        assert!(!is_valid_mac("aa-bb-cc-dd-ee-ff"));
    }

    #[test]
    fn rejects_non_hex_characters() {
        assert!(!is_valid_mac("zz:bb:cc:dd:ee:ff"));
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(!is_valid_mac("aa:bb:cc:dd:ee")); // too short
        assert!(!is_valid_mac("aa:bb:cc:dd:ee:ff:11")); // too long
    }

    #[test]
    fn rejects_control_or_newline_bytes() {
        // %0a (\n) injection attempt that flowed through Phase 5 into the
        // returned iPXE script — must be rejected.
        assert!(!is_valid_mac("aa:bb:cc:dd:ee:f\n"));
    }
}
