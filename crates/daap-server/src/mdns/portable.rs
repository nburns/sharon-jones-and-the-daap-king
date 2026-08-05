//! Cross-platform (non-macOS) Bonjour advertisement via the pure-Rust
//! `mdns-sd` crate.

use std::collections::HashMap;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use super::require_valid_hostname;

const SERVICE_TYPE: &str = "_daap._tcp.local.";

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Hostname(#[from] super::InvalidHostname),
    #[error(transparent)]
    Mdns(#[from] mdns_sd::Error),
}

pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    pub fn start(name: &str, port: u16) -> Result<Self, StartError> {
        require_valid_hostname(name)?;
        let daemon = ServiceDaemon::new()?;
        let hostname = format!("{}.local.", name);

        let mut txt = HashMap::<String, String>::new();
        txt.insert("txtvers".into(), "1".into());
        txt.insert("Version".into(), "196610".into());
        txt.insert("iTSh Version".into(), "196618".into());
        txt.insert("Password".into(), "false".into());
        txt.insert("Machine Name".into(), name.into());
        txt.insert("Machine ID".into(), "00000001".into());
        txt.insert("Database ID".into(), "00000001".into());

        let service = ServiceInfo::new(SERVICE_TYPE, name, &hostname, "", port, Some(txt))?
            .enable_addr_auto();

        let fullname = service.get_fullname().to_string();
        daemon.register(service)?;
        Ok(Self { daemon, fullname })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
