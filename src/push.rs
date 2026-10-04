//! Push notifications: the phones this machine sends to, and the relay it
//! sends through.
//!
//! The relay (`legio-relay`) is the only holder of the APNs key. A phone
//! registers there and gets a device secret, then hands the secret to this
//! machine with `legio push add`. This machine keeps the secrets in
//! `~/.config/legio/push.json` and never sees the key or the APNs token.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Where pushes go. `LEGIO_RELAY` overrides it, for a relay of your own.
pub const RELAY_URL: &str = "https://legiorelay.jrobe.cloud";

pub fn relay_url() -> String {
    std::env::var("LEGIO_RELAY")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| RELAY_URL.to_string())
}

/// One phone that asked for notifications.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub secret: String,
    /// The app's id for its connection to this machine. Each push carries
    /// it, so a tap opens this machine and not another.
    pub connection_id: String,
    /// The phone's name, for `legio push list`.
    #[serde(default)]
    pub name: String,
}

impl Device {
    /// Enough of the secret to tell two phones apart, and not enough to
    /// send as them.
    pub fn short_secret(&self) -> &str {
        &self.secret[..self.secret.len().min(8)]
    }
}

pub fn file(home: &Path) -> PathBuf {
    home.join(".config/legio/push.json")
}

pub fn load(path: &Path) -> anyhow::Result<Vec<Device>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("could not read {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Writes the list with mode 600: a secret lets anyone push to that phone.
pub fn save(path: &Path, devices: &[Device]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    f.write_all(&serde_json::to_vec_pretty(devices)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))
}

/// Adds a phone, or updates the one with the same secret. Returns whether
/// it was new.
pub fn upsert(devices: &mut Vec<Device>, device: Device) -> bool {
    match devices.iter_mut().find(|d| d.secret == device.secret) {
        Some(old) => {
            *old = device;
            false
        }
        None => {
            devices.push(device);
            true
        }
    }
}

/// Checks a secret the way the relay makes them: base64url, no padding.
pub fn check_secret(secret: &str) -> anyhow::Result<()> {
    let ok = (20..=100).contains(&secret.len())
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        bail!("that is not a device secret from the relay");
    }
    Ok(())
}

/// How urgent a push is. iOS shows a `TimeSensitive` one through Focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Active,
    TimeSensitive,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Active => "active",
            Level::TimeSensitive => "time-sensitive",
        }
    }
}

pub struct Message<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub level: Level,
    pub pane_id: Option<&'a str>,
    /// The agent status the push is about: `blocked` or `done`. The relay
    /// holds each phone's rules, and drops a push they do not allow. A
    /// push with no status — a test — always goes.
    pub status: Option<&'a str>,
    /// How long the agent's task ran, for the rules' minimum work time.
    pub worked: Option<Duration>,
}

/// What the relay said about one phone.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Ok,
    /// The phone's rules do not allow this push. Nothing to do.
    Skipped,
    /// The relay does not know the secret, or the phone is gone. Forget it.
    Gone,
}

pub struct Relay {
    url: String,
    agent: ureq::Agent,
}

impl Relay {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            // A 4xx is an answer to read, not an error to bail on.
            .http_status_as_error(false)
            .build();
        Self {
            url: relay_url(),
            agent: config.into(),
        }
    }

    pub fn send(&self, device: &Device, message: &Message) -> anyhow::Result<Sent> {
        let mut body = serde_json::json!({
            "title": message.title,
            "body": message.body,
            "level": message.level.as_str(),
            "connectionId": device.connection_id,
        });
        if let Some(pane) = message.pane_id {
            body["paneId"] = pane.into();
        }
        if let Some(status) = message.status {
            body["status"] = status.into();
        }
        if let Some(worked) = message.worked {
            body["workedSeconds"] = worked.as_secs().into();
        }
        let mut response = self
            .agent
            .post(format!("{}/v1/notify", self.url))
            .header("authorization", format!("Bearer {}", device.secret))
            .send_json(&body)
            .with_context(|| format!("could not reach the relay at {}", self.url))?;
        match response.status().as_u16() {
            204 => Ok(Sent::Ok),
            // A relay that kept the push back says so in the body. An older
            // relay answers 200 only for a push it sent.
            200 => {
                let reply: serde_json::Value = response.body_mut().read_json().unwrap_or_default();
                Ok(if reply["sent"] == false {
                    Sent::Skipped
                } else {
                    Sent::Ok
                })
            }
            410 => Ok(Sent::Gone),
            status => {
                let text = response.body_mut().read_to_string().unwrap_or_default();
                bail!("the relay answered {status}: {}", text.trim())
            }
        }
    }
}

/// Sends one message to every phone in the list, and drops the phones the
/// relay calls gone. Returns how many got it.
pub fn send_all(path: &Path, relay: &Relay, message: &Message) -> anyhow::Result<usize> {
    let mut sent = 0;
    let mut gone = Vec::new();
    for device in load(path)? {
        match relay.send(&device, message) {
            Ok(Sent::Ok) => sent += 1,
            Ok(Sent::Skipped) => {}
            Ok(Sent::Gone) => {
                eprintln!(
                    "legio: the relay no longer knows {} ({}). Removed it.",
                    device.name,
                    device.short_secret()
                );
                gone.push(device.secret);
            }
            // Kept: the relay or the network may be down for a moment.
            Err(e) => eprintln!("legio: push to {} failed: {e:#}", device.name),
        }
    }
    if !gone.is_empty() {
        // Read again before the write, so a `push add` that ran while this
        // was sending is not lost.
        let mut now = load(path)?;
        now.retain(|d| !gone.contains(&d.secret));
        save(path, &now)?;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(secret: &str, connection: &str) -> Device {
        Device {
            secret: secret.into(),
            connection_id: connection.into(),
            name: "phone".into(),
        }
    }

    #[test]
    fn upsert_adds_then_updates_by_secret() {
        let mut list = Vec::new();
        assert!(upsert(&mut list, device("a", "c1")));
        assert!(upsert(&mut list, device("b", "c1")));
        assert!(!upsert(&mut list, device("a", "c2")));
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].connection_id, "c2");
    }

    #[test]
    fn checks_secret_shape() {
        assert!(check_secret("4bdhpjaLlBQ68G5j8_SGmJHUL6Ysj28qYoLDmU5BsME").is_ok());
        assert!(check_secret("short").is_err());
        assert!(check_secret("has space in it and is long enough").is_err());
    }

    #[test]
    fn save_and_load_round_trip_with_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("legio-push-{}", std::process::id()));
        let path = dir.join("push.json");
        let list = vec![device("a", "c1")];
        save(&path, &list).unwrap();
        assert_eq!(load(&path).unwrap(), list);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
