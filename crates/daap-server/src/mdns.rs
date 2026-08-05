//! Bonjour/mDNS advertisement of the DAAP service.
//!
//! On macOS we delegate to the system `dns-sd` CLI because the pure-Rust
//! `mdns-sd` crate loses to macOS's system mDNSResponder for UDP 5353 — the
//! resulting advertisement is visible to local queries but not multicast onto
//! the LAN, so old iTunes clients on other hosts never see the A record for
//! our hostname.  On other platforms we keep the in-process `mdns-sd` path.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Advertisement;

#[cfg(not(target_os = "macos"))]
mod portable;
#[cfg(not(target_os = "macos"))]
pub use portable::Advertisement;

/// Validate that a name is usable as-is as a DNS label (ASCII alphanumeric
/// plus `-`). Returns an error rather than mangling the input so bad names
/// surface at launch instead of causing silent mDNS resolution failures.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn require_valid_hostname(name: &str) -> Result<(), InvalidHostname> {
    if name.is_empty() {
        return Err(InvalidHostname { name: name.into(), reason: "empty" });
    }
    if let Some(bad) = name.chars().find(|c| !c.is_ascii_alphanumeric() && *c != '-') {
        return Err(InvalidHostname {
            name: name.into(),
            reason: match bad {
                ' ' => "contains a space",
                _ => "contains an illegal character",
            },
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("invalid hostname {name:?}: {reason}")]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub struct InvalidHostname {
    pub name: String,
    pub reason: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_valid_input_ok() {
        assert!(require_valid_hostname("mylibrary").is_ok());
        assert!(require_valid_hostname("nick-music-01").is_ok());
    }

    #[test]
    fn hostname_rejects_space() {
        let e = require_valid_hostname("My Music").unwrap_err();
        assert!(e.reason.contains("space"), "{}", e);
    }

    #[test]
    fn hostname_rejects_empty() {
        assert!(require_valid_hostname("").is_err());
    }

    #[test]
    fn hostname_rejects_punct() {
        assert!(require_valid_hostname("nick's").is_err());
        assert!(require_valid_hostname("cool!").is_err());
    }
}
