//! Facts about this machine's SSH server that the pairing code carries,
//! and the checks that catch a login that will fail before the phone does.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::sshkey;
use crate::sys;
use crate::ui::{info, warn};

/// Which address goes in the QR code, and why.
pub enum Address {
    Given(String),
    /// The best answer: the phone reaches it without port 22 open to the
    /// internet.
    Tailscale(String),
    Public(String),
    Unknown,
}

impl Address {
    pub fn find(given: Option<&str>) -> Self {
        if let Some(host) = given {
            return Address::Given(host.into());
        }
        if let Some(ip) = sys::output("tailscale", &["ip", "-4"])
            .and_then(|o| o.lines().next().map(str::to_string))
        {
            return Address::Tailscale(ip);
        }
        // Through curl rather than an HTTP client here, so the tool carries
        // no TLS stack for one request.
        match sys::output(
            "curl",
            &["-fsS", "--max-time", "5", "https://api.ipify.org"],
        ) {
            Some(ip) => Address::Public(ip),
            None => Address::Unknown,
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Address::Given(h) | Address::Tailscale(h) | Address::Public(h) => h,
            Address::Unknown => "CHANGE-ME",
        }
    }

    pub fn report(&self) {
        match self {
            Address::Given(h) => info(&format!("Using the host you gave: {h}")),
            Address::Tailscale(h) => {
                info(&format!("Using the Tailscale address: {h}"));
                info("Keep port 22 closed to the internet. Run the Tailscale app on the phone.");
            }
            Address::Public(h) => {
                info(&format!("Using the public IP: {h}"));
                warn("This exposes port 22, and the pairing port for a moment, to the");
                warn("internet. Tailscale is the safer route: install it, then pair again.");
            }
            Address::Unknown => warn("Could not find an address. Pass one with --host <addr>."),
        }
    }
}

/// The effective value of one sshd option, when `sudo` can ask sshd
/// without a password prompt. `None` means "could not ask", not "off".
pub fn sshd_option(name: &str) -> Option<String> {
    let config = sys::output("sudo", &["-n", "sshd", "-T"])?;
    config.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == name).then(|| value.to_string())
    })
}

/// The port sshd listens on: from `sshd -T` when it can be asked, else the
/// first `Port` line of the config, else 22.
pub fn ssh_port() -> u16 {
    if let Some(port) = sshd_option("port").and_then(|p| p.parse().ok()) {
        return port;
    }
    fs::read_to_string("/etc/ssh/sshd_config")
        .ok()
        .and_then(|config| {
            config.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                let key = words.next()?;
                key.eq_ignore_ascii_case("port")
                    .then(|| words.next()?.parse().ok())?
            })
        })
        .unwrap_or(22)
}

/// The fingerprint of this machine's ed25519 host key. The app pins it, so
/// a machine in the middle cannot pose as this one after pairing. The
/// public half of a host key is readable by everyone, so this needs no
/// `sudo`.
pub fn host_key_fingerprint() -> Option<String> {
    let line = fs::read_to_string("/etc/ssh/ssh_host_ed25519_key.pub").ok()?;
    let blob = line.split_whitespace().nth(1)?;
    sshkey::fingerprint_of_blob(blob).ok()
}

/// sshd refuses every key in a home directory that the group or others can
/// write to, and says so only in its own log. Take the permission away
/// before it costs an hour.
pub fn fix_home_permissions(home: &Path) {
    let Ok(meta) = fs::metadata(home) else { return };
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        if fs::set_permissions(home, fs::Permissions::from_mode(mode & !0o022)).is_ok() {
            info(&format!(
                "Took group and other write off {}, which sshd requires.",
                home.display()
            ));
        } else {
            warn(&format!(
                "{} is writable by others, and sshd will refuse the key.",
                home.display()
            ));
            warn("Fix with: chmod go-w ~");
        }
    }
}

/// Checks the sshd settings that decide whether a key login can work.
pub fn check_sshd() {
    let Some(pubkey) = sshd_option("pubkeyauthentication") else {
        info("Could not read the sshd configuration without a password prompt.");
        info("Check it by hand: sudo sshd -T | grep -E 'pubkey|password|authorizedkeysfile'");
        return;
    };
    if pubkey == "yes" {
        info("PubkeyAuthentication is on. The app can log in with its key.");
    } else {
        warn("PubkeyAuthentication is off. Set 'PubkeyAuthentication yes' in");
        warn("/etc/ssh/sshd_config, then reload the SSH server.");
    }

    // This tool writes ~/.ssh/authorized_keys. A server that reads keys
    // from somewhere else would ignore every phone this pairs.
    if let Some(files) = sshd_option("authorizedkeysfile")
        && !files
            .split_whitespace()
            .any(|f| f == ".ssh/authorized_keys" || f == "%h/.ssh/authorized_keys")
    {
        warn(&format!(
            "sshd reads keys from '{files}', not ~/.ssh/authorized_keys."
        ));
        warn("Phones paired here will not be able to log in.");
    }

    if sshd_option("passwordauthentication").as_deref() == Some("yes") {
        warn("PasswordAuthentication is still on. The app does not need it.");
        warn("Turn it off once a phone logs in, so port 22 stops accepting passwords:");
        warn(
            "  echo 'PasswordAuthentication no' | sudo tee /etc/ssh/sshd_config.d/99-herdrpoc.conf",
        );
        warn("  sudo systemctl reload ssh");
    }
}
