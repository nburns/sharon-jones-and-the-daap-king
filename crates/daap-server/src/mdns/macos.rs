//! macOS Bonjour advertisement via the system `dns-sd` CLI, spawned as a
//! child process. `dns-sd -R` keeps the registration alive as long as the
//! process runs; killing it withdraws the record.

use std::io;
use std::process::{Child, Command, Stdio};

pub struct Advertisement {
    child: Child,
    fullname: String,
}

impl Advertisement {
    pub fn start(name: &str, port: u16) -> io::Result<Self> {
        // TXT records shaped to look like OwnTone / iTunes' own share.
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
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
