//! The bridge on a Mac, where there is no systemd.
//!
//! `scripts/mac-setup.sh` installs socat with Homebrew and keeps it running
//! as a LaunchAgent. The script is built into the binary, so one file is
//! all a Mac needs, as on Linux.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::ui::{info, warn};

const SCRIPT: &str = include_str!("../scripts/mac-setup.sh");

pub const LABEL: &str = "com.legio.bridge";

pub fn agent_plist(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

pub fn log_file(home: &Path) -> PathBuf {
    home.join(format!("Library/Logs/{LABEL}.log"))
}

/// Runs the built-in script with the terminal attached, so Homebrew can
/// show its progress.
fn run(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("bash")
        .arg("-c")
        .arg(SCRIPT)
        .arg("mac-setup.sh")
        .args(args)
        .status()
        .context("could not start bash")?;
    if !status.success() {
        bail!("mac-setup.sh failed ({status})");
    }
    Ok(())
}

/// Checks Remote Login, installs socat, writes the LaunchAgent, and asks
/// Herdr about `agent.kinds`. The script prints steps 1 to 3.
pub fn install(port: u16, socket: &Path, session: &str, on_demand: bool) -> anyhow::Result<()> {
    let port = port.to_string();
    let socket = socket.to_string_lossy();
    let mut args = vec!["--port", &port, "--socket", &socket, "--session", session];
    if on_demand {
        args.push("--on-demand");
    }
    run(&args)
}

pub fn uninstall() -> anyhow::Result<()> {
    run(&["--uninstall"])
}

/// Says whether the LaunchAgent is installed and the port answers.
pub fn report_agent(home: &Path, target_port: u16) {
    let plist = agent_plist(home);
    if !plist.exists() {
        warn("No bridge LaunchAgent is installed. Run: legio pair");
        return;
    }
    info(&format!("LaunchAgent: {}", plist.display()));
    let addr = SocketAddr::from(([127, 0, 0, 1], target_port));
    if TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok() {
        info(&format!("The bridge answers on 127.0.0.1:{target_port}."));
    } else {
        warn(&format!(
            "Nothing answers on 127.0.0.1:{target_port}. Read the log with:"
        ));
        warn(&format!("  cat {}", log_file(home).display()));
    }
}
