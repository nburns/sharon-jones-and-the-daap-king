//! Cross-platform (non-macOS) Bonjour advertisement via the pure-Rust
//! `mdns-sd` crate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{DaemonStatus, ServiceDaemon, ServiceInfo, UnregisterStatus};

use super::require_valid_hostname;

const SERVICE_TYPE: &str = "_daap._tcp.local.";
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Hostname(#[from] super::InvalidHostname),
    #[error(transparent)]
    Mdns(#[from] mdns_sd::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("mdns-sd error: {0}")]
    Mdns(String),
    #[error("goodbye timed out")]
    Timeout,
}

impl From<mdns_sd::Error> for StopError {
    fn from(e: mdns_sd::Error) -> Self {
        StopError::Mdns(e.to_string())
    }
}

/// Lightweight handle for synchronous emergency teardown (panic hook, signal
/// handlers that can't use async). Cloneable so it can live in a global.
#[derive(Clone)]
pub struct TeardownHandle {
    daemon: ServiceDaemon,
    fullname: Arc<Mutex<Option<String>>>,
}

impl TeardownHandle {
    /// Sends an mDNS goodbye and shuts the daemon down synchronously.
    /// Best-effort: uses a hard ~500ms budget and logs if steps fail.
    pub fn emergency_stop(&self) {
        let budget = Duration::from_millis(500);
        let fullname = {
            let mut guard = self.fullname.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(name) = fullname {
            match self.daemon.unregister(&name) {
                Ok(rx) => {
                    if rx.recv_timeout(budget).is_err() {
                        tracing::warn!("mDNS unregister did not confirm within budget");
                    }
                }
                Err(e) => tracing::warn!("mDNS unregister failed: {e}"),
            }
        }
        match self.daemon.shutdown() {
            Ok(rx) => {
                if rx.recv_timeout(budget).is_err() {
                    tracing::warn!("mDNS shutdown did not confirm within budget");
                }
            }
            Err(e) => tracing::warn!("mDNS shutdown failed: {e}"),
        }
    }
}

pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: Arc<Mutex<Option<String>>>,
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
        Ok(Self {
            daemon,
            fullname: Arc::new(Mutex::new(Some(fullname))),
        })
    }

    pub fn fullname(&self) -> Option<String> {
        self.fullname
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn teardown_handle(&self) -> TeardownHandle {
        TeardownHandle {
            daemon: self.daemon.clone(),
            fullname: Arc::clone(&self.fullname),
        }
    }

    /// Sends an mDNS goodbye and shuts down the daemon, waiting for
    /// confirmation with a timeout. Prefer this over relying on `Drop`.
    pub async fn stop(self) -> Result<(), StopError> {
        let fullname = {
            let mut guard = self.fullname.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(name) = fullname {
            let rx = self.daemon.unregister(&name)?;
            match tokio::time::timeout(STOP_TIMEOUT, rx.recv_async()).await {
                Ok(Ok(UnregisterStatus::OK)) => {}
                Ok(Ok(UnregisterStatus::NotFound)) => {
                    tracing::warn!("mDNS unregister: service not found");
                }
                Ok(Err(_)) => return Err(StopError::Mdns("unregister channel closed".into())),
                Err(_) => return Err(StopError::Timeout),
            }
        }
        let rx = self.daemon.shutdown()?;
        match tokio::time::timeout(STOP_TIMEOUT, rx.recv_async()).await {
            Ok(Ok(DaemonStatus::Shutdown)) => {}
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return Err(StopError::Mdns("shutdown channel closed".into())),
            Err(_) => return Err(StopError::Timeout),
        }
        Ok(())
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let fullname = {
            let mut guard = self.fullname.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        // Only fires if stop() was not called - this is the best-effort fallback.
        if fullname.is_some() {
            tracing::warn!("Advertisement dropped without stop() - goodbye may not flush");
            let _ = fullname.as_deref().and_then(|n| self.daemon.unregister(n).ok());
            let _ = self.daemon.shutdown();
        }
    }
}
