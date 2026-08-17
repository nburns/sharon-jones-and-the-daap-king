//! macOS Bonjour advertisement via the system `dns-sd` CLI, spawned as a
//! child process. `dns-sd -R` keeps the registration alive as long as the
//! process runs; killing it withdraws the record.

use std::io;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const STOP_TIMEOUT: Duration = Duration::from_secs(2);
const EMERGENCY_BUDGET: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("dns-sd did not exit within timeout")]
    Timeout,
}

/// Lightweight handle for synchronous emergency teardown (panic hook, signal
/// handlers that can't use async). Cloneable so it can live in a global.
#[derive(Clone)]
pub struct TeardownHandle {
    pid: u32,
}

impl TeardownHandle {
    /// Sends SIGTERM to the dns-sd child and briefly waits for it to exit.
    /// Best-effort: hard-cap ~500ms budget; logs if steps fail.
    pub fn emergency_stop(&self) {
        // SAFETY: kill(2) is async-signal-safe and pid is a valid u32.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now();
        loop {
            // WNOHANG spin until the child exits or budget expires.
            let mut status: libc::c_int = 0;
            let ret =
                unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
            if ret > 0 {
                break; // child exited
            }
            if deadline.elapsed() >= EMERGENCY_BUDGET {
                tracing::warn!("dns-sd (pid={}) did not exit within emergency budget", self.pid);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub struct Advertisement {
    child: Child,
    fullname: String,
}

impl Advertisement {
    /// Advertises the DAAP service. `name` is the DNS-SD service instance
    /// name and may contain spaces.
    ///
    /// `hostname` is accepted for signature parity with the portable backend
    /// and deliberately ignored: `dns-sd -R` registers the service only, and
    /// mDNSResponder answers with the machine's own host record. There is no
    /// way to override it here, so callers on macOS always get the system
    /// hostname as the SRV target regardless of what they pass.
    pub fn start(name: &str, _hostname: &str, port: u16) -> io::Result<Self> {
        let txt = [
            ("txtvers", "1"),
            ("Version", "196610"),
            ("iTSh Version", "196618"),
            ("Password", "false"),
            ("Machine Name", name),
            ("Database ID", "00000001"),
            ("Machine ID", "00000001"),
        ];

        let mut cmd = Command::new("dns-sd");
        cmd.arg("-R")
            .arg(name)
            .arg("_daap._tcp")
            .arg("local")
            .arg(port.to_string());
        for (k, v) in txt {
            cmd.arg(format!("{k}={v}"));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let child = cmd.spawn()?;
        tracing::info!(name, port, "advertised via dns-sd (pid={})", child.id());
        Ok(Self {
            child,
            fullname: format!("{name}._daap._tcp.local."),
        })
    }

    pub fn fullname(&self) -> &str {
        &self.fullname
    }

    pub fn teardown_handle(&self) -> TeardownHandle {
        TeardownHandle { pid: self.child.id() }
    }

    /// Sends SIGTERM to dns-sd and waits for it to exit (mDNSResponder then
    /// sends a goodbye). Falls back to SIGKILL if the timeout expires.
    pub async fn stop(mut self) -> Result<(), StopError> {
        let pid = self.child.id();
        // SAFETY: kill(2) is safe to call from any thread.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        let result = tokio::time::timeout(
            STOP_TIMEOUT,
            tokio::task::spawn_blocking(move || self.child.wait()),
        )
        .await;

        match result {
            Ok(Ok(Ok(_status))) => Ok(()),
            Ok(Ok(Err(e))) => Err(StopError::Io(e)),
            Ok(Err(join_err)) => {
                Err(StopError::Io(io::Error::other(join_err.to_string())))
            }
            Err(_timeout) => {
                // SIGKILL as last resort; the goodbye is lost.
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                Err(StopError::Timeout)
            }
        }
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // Best-effort fallback - prefer stop().await for a clean goodbye.
        tracing::warn!(
            "Advertisement dropped without stop() - mDNSResponder may not send goodbye"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
