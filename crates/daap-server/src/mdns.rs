//! Bonjour/mDNS advertisement of the DAAP service.
//!
//! On macOS we delegate to the system `dns-sd` CLI because the pure-Rust
//! `mdns-sd` crate loses to macOS's system mDNSResponder for UDP 5353 - the
//! resulting advertisement is visible to local queries but not multicast onto
//! the LAN, so old iTunes clients on other hosts never see the A record for
//! our hostname.  On other platforms we keep the in-process `mdns-sd` path.
//!
//! # Instance name vs. hostname
//!
//! These are two different fields and must not be conflated. The service
//! instance name is free-form UTF-8 shown to users (RFC 6763 section 4.1.1);
//! the hostname is a DNS label the SRV record points at. macOS only lets us
//! set the former - mDNSResponder owns the host record - whereas the portable
//! backend has to publish both, so `Advertisement::start` takes them
//! separately and only the hostname is validated.
//!
//! # Teardown contract
//!
//! Prefer calling `Advertisement::stop().await` to ensure an mDNS goodbye
//! packet (TTL=0) is sent before the process exits. `Drop` is a best-effort
//! safety net and logs a warning if it fires in place of `stop()`.
//!
//! For synchronous exit paths (panic hooks, signal handlers), obtain a
//! `TeardownHandle` via `Advertisement::teardown_handle()` before handing
//! the `Advertisement` to the async loop, store it somewhere reachable, and
//! call `TeardownHandle::emergency_stop()`.
//!
//! SIGKILL, SIGSEGV, SIGBUS, and power loss cannot be handled; the record
//! will expire per its announced TTL.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{Advertisement, StopError, TeardownHandle};

#[cfg(not(target_os = "macos"))]
mod portable;
#[cfg(not(target_os = "macos"))]
pub use portable::{Advertisement, StopError, TeardownHandle};

/// Validate that a name is usable as-is as a DNS label (ASCII alphanumeric
/// plus `-`). Returns an error rather than mangling the input so bad names
/// surface at launch instead of causing silent mDNS resolution failures.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn require_valid_hostname(name: &str) -> Result<(), InvalidHostname> {
    if name.is_empty() {
        return Err(InvalidHostname {
            name: name.into(),
            reason: "empty",
        });
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && *c != '-')
    {
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

    /// Regression guard: the CLI's default library name is not a legal DNS
    /// label, so the instance name and the hostname must stay separate fields.
    #[test]
    fn default_library_name_is_not_a_valid_hostname() {
        assert!(require_valid_hostname("Classic iTunes Streamer").is_err());
    }

    #[test]
    fn hostname_rejects_punct() {
        assert!(require_valid_hostname("nick's").is_err());
        assert!(require_valid_hostname("cool!").is_err());
    }
}
