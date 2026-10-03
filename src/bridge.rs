//! The TCP bridge to Herdr's socket.
//!
//! The app makes one SSH connection to this machine. Over it, it opens a
//! direct-tcpip channel to 127.0.0.1:<target port>, where a small proxy
//! exposes Herdr's Unix socket as a TCP port. This module picks that proxy,
//! keeps it running as a systemd user unit, and asks Herdr through it
//! whether it is new enough for the app.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};

use crate::sys;
use crate::ui::{info, warn};

pub const UNIT_NAME: &str = "herdr-bridge";

pub enum Proxy {
    /// `systemd-socket-proxyd`, which ships with systemd: nothing to
    /// install, and systemd holds the socket.
    Proxyd(PathBuf),
    Socat(PathBuf),
}

impl Proxy {
    pub fn path(&self) -> &Path {
        match self {
            Proxy::Proxyd(p) | Proxy::Socat(p) => p,
        }
    }
}

pub fn unit_dir(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
}

pub fn require_systemd() -> anyhow::Result<()> {
    if !Path::new("/run/systemd/system").is_dir() || sys::which("systemctl").is_none() {
        bail!("systemd is not running. The bridge needs systemd (a Linux server)");
    }
    Ok(())
}

fn find_socket_proxyd() -> Option<PathBuf> {
    [
        "/usr/lib/systemd/systemd-socket-proxyd",
        "/lib/systemd/systemd-socket-proxyd",
        "/usr/libexec/systemd/systemd-socket-proxyd",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| sys::is_executable(p))
    .or_else(|| sys::which("systemd-socket-proxyd"))
}

/// systemd's own proxy first, socat second. socat is installed only when
/// systemd has no proxy, or when `--use-socat` asks for it.
pub fn choose_proxy(use_socat: bool) -> anyhow::Result<Proxy> {
    if !use_socat && let Some(path) = find_socket_proxyd() {
        info(&format!(
            "Proxy: {} (ships with systemd — nothing to install).",
            path.display()
        ));
        return Ok(Proxy::Proxyd(path));
    }
    let path = match sys::which("socat") {
        Some(path) => path,
        None => {
            info("systemd has no socket proxy here. Installing socat.");
            sys::install_package("socat", "socat")?;
            sys::which("socat").context("socat is still missing after the install")?
        }
    };
    info(&format!("Proxy: {}", path.display()));
    Ok(Proxy::Socat(path))
}

fn systemctl(args: &[&str]) -> bool {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    sys::succeeds("systemctl", &all)
}

/// Lingering keeps the user's units running after they log out — without
/// it the bridge stops when the SSH session that ran this tool ends.
pub fn ensure_linger(user: &str) {
    if sys::which("loginctl").is_none() {
        return;
    }
    if sys::output("loginctl", &["show-user", user, "-p", "Linger", "--value"]).as_deref()
        == Some("yes")
    {
        info(&format!("Lingering is already on for {user}."));
        return;
    }
    info("Turning on lingering, so the units run when you are logged out.");
    if sys::run("sudo", &["loginctl", "enable-linger", user]).is_err() {
        warn("Could not turn on lingering. The units stop when you log out.");
    }
}

/// Stops and deletes both kinds of unit, so a switch from one proxy to the
/// other never leaves two listening on the port.
pub fn remove_units(home: &Path) {
    let dir = unit_dir(home);
    for unit in [
        format!("{UNIT_NAME}.socket"),
        format!("{UNIT_NAME}.service"),
    ] {
        systemctl(&["disable", "--now", &unit]);
        let _ = fs::remove_file(dir.join(&unit));
    }
    systemctl(&["daemon-reload"]);
}

/// Writes the units, starts them, and returns the name of the one that
/// was enabled.
pub fn install_units(
    home: &Path,
    proxy: &Proxy,
    target_port: u16,
    socket: &Path,
) -> anyhow::Result<String> {
    let dir = unit_dir(home);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    remove_units(home);

    let proxy_bin = proxy.path().display();
    let socket = socket.display();
    let enable = match proxy {
        Proxy::Proxyd(_) => {
            // systemd holds the listening socket and starts the proxy on
            // the first connection. The app opens one connection per RPC
            // call; one proxy process serves them all.
            fs::write(
                dir.join(format!("{UNIT_NAME}.socket")),
                format!(
                    "[Unit]\nDescription=TCP front for the Herdr socket (Legio app)\n\n\
                     [Socket]\nListenStream=127.0.0.1:{target_port}\n\n\
                     [Install]\nWantedBy=sockets.target\n"
                ),
            )?;
            fs::write(
                dir.join(format!("{UNIT_NAME}.service")),
                format!(
                    "[Unit]\nDescription=Proxy 127.0.0.1:{target_port} to the Herdr socket (Legio app)\n\
                     Requires={UNIT_NAME}.socket\nAfter={UNIT_NAME}.socket\n\n\
                     [Service]\nExecStart={proxy_bin} {socket}\n"
                ),
            )?;
            info(&format!(
                "Wrote {}/{UNIT_NAME}.socket and .service.",
                dir.display()
            ));
            format!("{UNIT_NAME}.socket")
        }
        Proxy::Socat(_) => {
            fs::write(
                dir.join(format!("{UNIT_NAME}.service")),
                format!(
                    "[Unit]\nDescription=Expose the Herdr socket as TCP 127.0.0.1:{target_port} (Legio app)\n\
                     After=default.target\n\n\
                     [Service]\nExecStart={proxy_bin} TCP-LISTEN:{target_port},bind=127.0.0.1,reuseaddr,fork UNIX-CONNECT:{socket}\n\
                     Restart=always\nRestartSec=2\n\n\
                     [Install]\nWantedBy=default.target\n"
                ),
            )?;
            info(&format!("Wrote {}/{UNIT_NAME}.service.", dir.display()));
            format!("{UNIT_NAME}.service")
        }
    };

    systemctl(&["daemon-reload"]);
    if !systemctl(&["enable", "--now", &enable]) {
        bail!("could not start {enable}. Read the log with: systemctl --user status {enable}");
    }
    std::thread::sleep(Duration::from_secs(1));
    report_unit(&enable, target_port);
    Ok(enable)
}

/// Says whether the bridge unit is up. The socket unit when there is one,
/// because with systemd's proxy the service only runs while a connection
/// is open.
pub fn report_unit(unit: &str, target_port: u16) {
    if systemctl(&["is-active", "--quiet", unit]) {
        info(&format!("{unit} is active on 127.0.0.1:{target_port}."));
    } else {
        warn(&format!("{unit} is not active. Read the log with:"));
        warn(&format!("  systemctl --user status {unit}"));
    }
}

/// Whether a bridge is installed: the systemd units, or the LaunchAgent on
/// a Mac.
pub fn is_installed(home: &Path) -> bool {
    if cfg!(target_os = "macos") {
        crate::mac::agent_plist(home).exists()
    } else {
        installed_unit(home).is_some()
    }
}

/// Whether something listens on the bridge port.
pub fn answers(target_port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], target_port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok()
}

/// The unit that runs the bridge now, if any.
pub fn installed_unit(home: &Path) -> Option<String> {
    let dir = unit_dir(home);
    [
        format!("{UNIT_NAME}.socket"),
        format!("{UNIT_NAME}.service"),
    ]
    .into_iter()
    .find(|unit| dir.join(unit).exists())
}

/// Asks the running Herdr whether it knows `agent.kinds`, the one method
/// the app needs that older builds do not have. Without it the app's
/// new-tab sheet cannot list the agent harnesses, and says so.
///
/// Deliberately not a version-number check: the build that answers
/// `agent.kinds` and the build that rejects it both call themselves 0.9.0
/// — the method arrived on the preview channel, not in a new version
/// number. The method is the fact, so the method is what this asks.
///
/// Spoken over the bridge itself, so it proves the bridge too. Herdr
/// serves one request per connection, so one connection is all this opens.
pub fn probe_agent_kinds(target_port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], target_port));
    let reply = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).and_then(|mut stream| {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(b"{\"id\":\"setup\",\"method\":\"agent.kinds\",\"params\":{}}\n")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    });

    let reply = match reply {
        Ok(reply) => reply,
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            warn("Herdr did not answer within 5 seconds. Make sure the server");
            warn("is running, then run this again.");
            return;
        }
        Err(_) => {
            warn(&format!(
                "Could not open 127.0.0.1:{target_port}, so the Herdr version"
            ));
            warn("stays unchecked.");
            return;
        }
    };

    if reply.contains("\"result\"") {
        info("Herdr answers agent.kinds. The app can list agent harnesses.");
    } else if reply.trim().is_empty() {
        warn("Herdr closed the connection without an answer. Make sure the");
        warn("server is running, then run this again.");
    } else if reply.contains("unknown variant") || reply.contains("\"error\"") {
        warn("This Herdr does not know the 'agent.kinds' method.");
        warn("Everything else works. Only the new-tab sheet is affected:");
        warn("it cannot list the agent harnesses, so it offers a plain");
        warn("shell and says 'Could not list agents'.");
        warn("The method is on the preview channel. To get it:");
        warn("  herdr channel set preview");
        warn("  herdr update");
        warn("  herdr server stop     # the running server keeps the old binary");
        warn("Then start Herdr again and run this again.");
    } else {
        warn("Herdr gave an answer this tool does not recognise:");
        warn(&format!("  {}", reply.trim()));
    }
}

/// Reports where `herdr` is and which build, or that the login shell will
/// not find it.
pub fn report_herdr(session: &str) {
    match sys::which("herdr") {
        Some(path) => {
            info(&format!("herdr: {}", path.display()));
            let version = sys::output("herdr", &["--version"]).unwrap_or_default();
            info(&format!(
                "Version: {}",
                version.trim_start_matches("herdr ")
            ));
            let channel =
                sys::output("herdr", &["channel", "show"]).unwrap_or_else(|| "unknown".into());
            info(&format!("Update channel: {channel}"));
        }
        None => {
            warn("herdr is not on PATH for this shell.");
            warn(&format!(
                "The app types 'herdr session attach {session}' in a login shell."
            ));
            warn("Put herdr on the PATH of your login shell before you attach to a pane.");
        }
    }
}

pub fn report_socket(socket: &Path) {
    use std::os::unix::fs::FileTypeExt;
    if fs::metadata(socket).is_ok_and(|m| m.file_type().is_socket()) {
        info(&format!("Socket: {}", socket.display()));
    } else {
        warn(&format!("No socket at {}.", socket.display()));
        warn("Start the Herdr server first. The TCP port opens anyway, but the");
        warn("app cannot connect until the socket exists.");
    }
}
