//! `legio watch`: tells the paired phones when an agent needs them.
//!
//! The phone cannot see a Herdr event while iOS has the app suspended, so
//! this machine watches for it. It reads Herdr's `session.snapshot` every
//! few seconds over the socket and sends a push through the relay when an
//! agent turns `blocked` (it waits for input) or `done` (it finished).
//!
//! A snapshot, not `events.subscribe`: the event that carries an agent's
//! status, `pane.agent_status_changed`, needs a `pane_id` up front and so
//! cannot be subscribed to for every pane at once. A local snapshot every
//! two seconds costs nothing, and sees every pane.
//!
//! It runs as a service of its own: a systemd user unit on Linux, a
//! LaunchAgent on a Mac. The bridge itself is a plain proxy with no
//! `legio` process behind it, so nothing else is there to watch.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};

use crate::push::{self, Level, Message, Relay};
use crate::sys;
use crate::ui::{info, warn};

const POLL: Duration = Duration::from_secs(2);
/// How long to wait before trying a socket that did not answer again.
const RETRY: Duration = Duration::from_secs(10);

pub const UNIT_NAME: &str = "legio-watch";
pub const LABEL: &str = "com.legio.watch";

// MARK: - What Herdr says

/// One agent, as the snapshot's `agents` array gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub pane_id: String,
    pub agent: String,
    pub status: String,
    /// When the status began. Two `done`s with different times are two
    /// finished turns, and both are worth a push.
    pub since: Option<u64>,
    pub title: String,
    pub workspace: String,
}

/// Reads one snapshot. Herdr answers one request per connection, so each
/// call dials again.
pub fn snapshot(socket: &Path) -> anyhow::Result<Vec<Agent>> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("could not connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream
        .write_all(b"{\"id\":\"legio-watch\",\"method\":\"session.snapshot\",\"params\":{}}\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).context("Herdr sent a line that is not JSON")?;
    if let Some(error) = reply.get("error") {
        bail!("session.snapshot: {error}");
    }
    Ok(parse_agents(&reply["result"]["snapshot"]))
}

fn parse_agents(snapshot: &serde_json::Value) -> Vec<Agent> {
    let labels: HashMap<&str, &str> = snapshot["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| Some((w["workspace_id"].as_str()?, w["label"].as_str()?)))
        .collect();
    snapshot["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| {
            let workspace_id = a["workspace_id"].as_str().unwrap_or_default();
            Some(Agent {
                pane_id: a["pane_id"].as_str()?.to_string(),
                agent: a["agent"].as_str().unwrap_or("agent").to_string(),
                status: a["agent_status"].as_str()?.to_string(),
                since: a["status_since_unix_ms"].as_u64(),
                title: a["terminal_title_stripped"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                workspace: labels
                    .get(workspace_id)
                    .copied()
                    .unwrap_or(workspace_id)
                    .to_string(),
            })
        })
        .collect()
}

// MARK: - Deciding what to send

/// The moment one agent asked for a person: its status and when it began.
type Key = (String, Option<u64>);

/// Finds the agents that turned `blocked` or `done`.
///
/// An agent must show the same status in two snapshots in a row before it
/// counts: an agent that blocks for a moment and goes on by itself is not
/// worth a phone in your hand. Each moment is sent once.
#[derive(Default)]
pub struct Detector {
    last: HashMap<String, Key>,
    sent: HashMap<String, Key>,
    started: bool,
}

impl Detector {
    pub fn next(&mut self, agents: &[Agent]) -> Vec<Agent> {
        let mut out = Vec::new();
        let mut now = HashMap::new();
        for agent in agents {
            let key = (agent.status.clone(), agent.since);
            let wants_person = matches!(agent.status.as_str(), "blocked" | "done");
            if wants_person {
                if !self.started {
                    // What was already waiting when the watcher started is
                    // old news, not a new moment.
                    self.sent.insert(agent.pane_id.clone(), key.clone());
                } else if self.last.get(&agent.pane_id) == Some(&key)
                    && self.sent.get(&agent.pane_id) != Some(&key)
                {
                    self.sent.insert(agent.pane_id.clone(), key.clone());
                    out.push(agent.clone());
                }
            }
            now.insert(agent.pane_id.clone(), key);
        }
        // A closed pane leaves nothing to remember.
        self.sent.retain(|pane, _| now.contains_key(pane));
        self.last = now;
        self.started = true;
        out
    }
}

/// The words on the lock screen.
fn message(agent: &Agent) -> (String, String, Level) {
    let name = capitalized(&agent.agent);
    let (title, level) = if agent.status == "blocked" {
        (format!("{name} needs input"), Level::TimeSensitive)
    } else {
        (format!("{name} finished"), Level::Active)
    };
    let body = match (agent.workspace.is_empty(), agent.title.is_empty()) {
        (false, false) => format!("{} · {}", agent.workspace, agent.title),
        (false, true) => agent.workspace.clone(),
        (true, false) => agent.title.clone(),
        (true, true) => sys::hostname(),
    };
    (title, body, level)
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// MARK: - The loop

/// Runs until killed. The service manager restarts it if it dies.
pub fn run(socket: &Path, home: &Path) -> anyhow::Result<()> {
    let devices = push::file(home);
    let relay = Relay::new();
    let mut detector = Detector::default();
    let mut socket_down = false;
    eprintln!(
        "legio watch: reading {} every {}s, sending through {}",
        socket.display(),
        POLL.as_secs(),
        push::relay_url()
    );
    loop {
        let agents = match snapshot(socket) {
            Ok(agents) => {
                if socket_down {
                    eprintln!("legio watch: Herdr answers again.");
                    socket_down = false;
                }
                agents
            }
            Err(e) => {
                if !socket_down {
                    eprintln!(
                        "legio watch: {e:#}. Trying again every {}s.",
                        RETRY.as_secs()
                    );
                    socket_down = true;
                }
                // Herdr restarted: its pane ids may be new, and what was
                // waiting before is not a new moment.
                detector = Detector::default();
                std::thread::sleep(RETRY);
                continue;
            }
        };
        for agent in detector.next(&agents) {
            let (title, body, level) = message(&agent);
            let sent = push::send_all(
                &devices,
                &relay,
                &Message {
                    title: &title,
                    body: &body,
                    level,
                    pane_id: Some(&agent.pane_id),
                },
            );
            match sent {
                Ok(n) => eprintln!(
                    "legio watch: {title} ({}), sent to {n} phone(s).",
                    agent.pane_id
                ),
                Err(e) => eprintln!("legio watch: {e:#}"),
            }
        }
        std::thread::sleep(POLL);
    }
}

// MARK: - The service

fn exe() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("could not tell where this legio binary is")
}

fn unit_file(home: &Path) -> PathBuf {
    crate::bridge::unit_dir(home).join(format!("{UNIT_NAME}.service"))
}

pub fn agent_plist(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

pub fn log_file(home: &Path) -> PathBuf {
    home.join(format!("Library/Logs/{LABEL}.log"))
}

fn gui_target() -> anyhow::Result<String> {
    let uid = sys::output("id", &["-u"]).context("could not read the user id")?;
    Ok(format!("gui/{uid}"))
}

/// Whether this machine runs Herdr, or did: the watcher has nothing to
/// read anywhere else. The socket exists only while the server runs, so the
/// binary, its config folder and an installed bridge count too.
pub fn herdr_here(home: &Path, socket: &Path) -> bool {
    socket.exists()
        || sys::which("herdr").is_some()
        || home.join(".config/herdr").is_dir()
        || crate::bridge::is_installed(home)
}

/// Installs the watcher unless it already runs on `socket`. With
/// `same_binary`, a watcher that runs another `legio` binary is written
/// again too — `legio pair` asks for that, so a watcher left on a build
/// folder or an old place moves to the binary that ran it.
///
/// Returns whether it wrote the service.
pub fn ensure(home: &Path, socket: &Path, same_binary: bool) -> anyhow::Result<bool> {
    let args = service_args(home);
    let socket_same = args
        .as_deref()
        .and_then(socket_in_args)
        .is_some_and(|s| s == socket);
    let binary_same = !same_binary
        || args
            .as_deref()
            .and_then(|a| a.first())
            .is_some_and(|b| exe().is_ok_and(|exe| Path::new(b) == exe));
    if socket_same && binary_same && is_running() {
        info("The notification watcher is already running.");
        return Ok(false);
    }
    install(home, socket)?;
    Ok(true)
}

/// Whether the watcher runs. The service manager is asked first; the
/// process list second, because `push add` runs in the app's SSH session,
/// where a service manager may not answer for the user's own services.
fn is_running() -> bool {
    service_is_active() || sys::succeeds("pgrep", &["-f", "legio watch --socket"])
}

fn service_is_active() -> bool {
    if cfg!(target_os = "macos") {
        gui_target().is_ok_and(|t| sys::succeeds("launchctl", &["print", &format!("{t}/{LABEL}")]))
    } else {
        sys::succeeds(
            "systemctl",
            &[
                "--user",
                "is-active",
                "--quiet",
                &format!("{UNIT_NAME}.service"),
            ],
        )
    }
}

/// Writes the service that runs `legio watch` and starts it. It runs this
/// same binary, so move or reinstall legio and run `legio pair` again.
pub fn install(home: &Path, socket: &Path) -> anyhow::Result<()> {
    let exe = exe()?;
    if cfg!(target_os = "macos") {
        let plist = agent_plist(home);
        std::fs::create_dir_all(plist.parent().unwrap_or(home))?;
        let log = log_file(home);
        std::fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <plist version=\"1.0\">\n<dict>\n\
                 \t<key>Label</key>\n\t<string>{LABEL}</string>\n\
                 \t<key>ProgramArguments</key>\n\t<array>\n\
                 \t\t<string>{}</string>\n\t\t<string>watch</string>\n\
                 \t\t<string>--socket</string>\n\t\t<string>{}</string>\n\t</array>\n\
                 \t<key>RunAtLoad</key>\n\t<true/>\n\
                 \t<key>KeepAlive</key>\n\t<true/>\n\
                 \t<key>StandardOutPath</key>\n\t<string>{}</string>\n\
                 \t<key>StandardErrorPath</key>\n\t<string>{}</string>\n\
                 </dict>\n</plist>\n",
                xml(&exe.to_string_lossy()),
                xml(&socket.to_string_lossy()),
                xml(&log.to_string_lossy()),
                xml(&log.to_string_lossy()),
            ),
        )
        .with_context(|| format!("could not write {}", plist.display()))?;
        let target = gui_target()?;
        let _ = sys::succeeds("launchctl", &["bootout", &format!("{target}/{LABEL}")]);
        sys::run(
            "launchctl",
            &["bootstrap", &target, &plist.to_string_lossy()],
        )?;
        info(&format!("Wrote {} and started it.", plist.display()));
    } else {
        let unit = unit_file(home);
        std::fs::create_dir_all(crate::bridge::unit_dir(home))?;
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=Push notifications for Herdr agents (Legio app)\n\
                 After=default.target\n\n\
                 [Service]\nExecStart={} watch --socket {}\n\
                 Restart=always\nRestartSec=5\n\n\
                 [Install]\nWantedBy=default.target\n",
                exe.display(),
                socket.display()
            ),
        )
        .with_context(|| format!("could not write {}", unit.display()))?;
        let unit_name = format!("{UNIT_NAME}.service");
        let _ = sys::succeeds("systemctl", &["--user", "daemon-reload"]);
        if !sys::succeeds("systemctl", &["--user", "enable", "--now", &unit_name]) {
            bail!(
                "could not start {unit_name}. Read the log with: journalctl --user -u {UNIT_NAME}"
            );
        }
        // A unit that was running picks up a new binary only on restart.
        let _ = sys::succeeds("systemctl", &["--user", "restart", &unit_name]);
        info(&format!("Wrote {} and started it.", unit.display()));
    }
    Ok(())
}

pub fn is_installed(home: &Path) -> bool {
    if cfg!(target_os = "macos") {
        agent_plist(home).exists()
    } else {
        unit_file(home).exists()
    }
}

/// The socket the installed watcher reads, from its service file.
pub fn installed_socket(home: &Path) -> Option<PathBuf> {
    socket_in_args(&service_args(home)?)
}

/// The command line in the installed service file: the binary first.
fn service_args(home: &Path) -> Option<Vec<String>> {
    if cfg!(target_os = "macos") {
        Some(args_in_plist(
            &std::fs::read_to_string(agent_plist(home)).ok()?,
        ))
    } else {
        args_in_unit(&std::fs::read_to_string(unit_file(home)).ok()?)
    }
}

fn socket_in_args(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--socket")?;
    args.get(at + 1).map(PathBuf::from)
}

/// The strings of the plist, which are the label first and then the
/// program's arguments — the label is dropped.
fn args_in_plist(plist: &str) -> Vec<String> {
    let Some(array) = plist
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("</array>").next())
    else {
        return Vec::new();
    };
    array
        .split("<string>")
        .skip(1)
        .filter_map(|part| part.split("</string>").next())
        .map(|text| {
            text.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .collect()
}

fn args_in_unit(unit: &str) -> Option<Vec<String>> {
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart="))?;
    Some(exec.split_whitespace().map(str::to_string).collect())
}

pub fn remove(home: &Path) {
    if cfg!(target_os = "macos") {
        if let Ok(target) = gui_target() {
            let _ = sys::succeeds("launchctl", &["bootout", &format!("{target}/{LABEL}")]);
        }
        let _ = std::fs::remove_file(agent_plist(home));
    } else {
        let unit_name = format!("{UNIT_NAME}.service");
        let _ = sys::succeeds("systemctl", &["--user", "disable", "--now", &unit_name]);
        let _ = std::fs::remove_file(unit_file(home));
        let _ = sys::succeeds("systemctl", &["--user", "daemon-reload"]);
    }
}

/// Says whether the watcher runs, and how many phones it sends to.
pub fn report(home: &Path) {
    if cfg!(target_os = "macos") {
        if !agent_plist(home).exists() {
            warn("The notification watcher is not installed. Run: legio pair");
        } else if gui_target()
            .is_ok_and(|t| sys::succeeds("launchctl", &["print", &format!("{t}/{LABEL}")]))
        {
            info(&format!(
                "{LABEL} is loaded. Log: {}",
                log_file(home).display()
            ));
        } else {
            warn(&format!(
                "{LABEL} is installed but not loaded. Run: legio pair"
            ));
        }
    } else if !unit_file(home).exists() {
        warn("The notification watcher is not installed. Run: legio pair");
    } else if sys::succeeds(
        "systemctl",
        &[
            "--user",
            "is-active",
            "--quiet",
            &format!("{UNIT_NAME}.service"),
        ],
    ) {
        info(&format!("{UNIT_NAME}.service is active."));
    } else {
        warn(&format!(
            "{UNIT_NAME}.service is not active. Read the log with:"
        ));
        warn(&format!("  journalctl --user -u {UNIT_NAME}"));
    }
    match push::load(&push::file(home)) {
        Ok(devices) if devices.is_empty() => {
            info("No phone gets notifications yet. Pair one with: legio pair")
        }
        Ok(devices) => info(&format!(
            "Sends to {} phone(s) through {}.",
            devices.len(),
            push::relay_url()
        )),
        Err(e) => warn(&format!("{e:#}")),
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, status: &str, since: u64) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent: "claude".into(),
            status: status.into(),
            since: Some(since),
            title: "fix the build".into(),
            workspace: "legio".into(),
        }
    }

    #[test]
    fn reads_the_command_line_from_the_service_files() {
        let plist = "<key>Label</key>\n<string>com.legio.watch</string>\n\
                     <key>ProgramArguments</key>\n<array>\n<string>/old/legio</string>\n<string>watch</string>\n\
                     <string>--socket</string>\n<string>/home/a &amp; b/herdr.sock</string>\n</array>\n\
                     <key>StandardOutPath</key>\n<string>/tmp/log</string>";
        let args = args_in_plist(plist);
        assert_eq!(args[0], "/old/legio");
        assert_eq!(
            socket_in_args(&args),
            Some(PathBuf::from("/home/a & b/herdr.sock"))
        );
        let unit = "[Service]\nExecStart=/old/legio watch --socket /home/u/.config/herdr/herdr.sock\nRestart=always\n";
        let args = args_in_unit(unit).unwrap();
        assert_eq!(args[0], "/old/legio");
        assert_eq!(
            socket_in_args(&args),
            Some(PathBuf::from("/home/u/.config/herdr/herdr.sock"))
        );
        assert_eq!(
            socket_in_args(&args_in_unit("[Service]\nExecStart=/old/legio watch\n").unwrap()),
            None
        );
    }

    #[test]
    fn ignores_what_was_waiting_at_start() {
        let mut d = Detector::default();
        assert!(d.next(&[agent("p1", "blocked", 1)]).is_empty());
        assert!(d.next(&[agent("p1", "blocked", 1)]).is_empty());
    }

    #[test]
    fn sends_once_after_two_snapshots() {
        let mut d = Detector::default();
        assert!(d.next(&[agent("p1", "working", 1)]).is_empty());
        assert!(d.next(&[agent("p1", "blocked", 2)]).is_empty());
        assert_eq!(d.next(&[agent("p1", "blocked", 2)]).len(), 1);
        assert!(d.next(&[agent("p1", "blocked", 2)]).is_empty());
    }

    #[test]
    fn a_short_block_is_not_sent() {
        let mut d = Detector::default();
        d.next(&[agent("p1", "working", 1)]);
        d.next(&[agent("p1", "blocked", 2)]);
        assert!(d.next(&[agent("p1", "working", 3)]).is_empty());
        assert!(d.next(&[agent("p1", "working", 3)]).is_empty());
    }

    #[test]
    fn a_new_turn_that_finishes_is_sent_again() {
        let mut d = Detector::default();
        d.next(&[agent("p1", "working", 1)]);
        d.next(&[agent("p1", "done", 2)]);
        assert_eq!(d.next(&[agent("p1", "done", 2)]).len(), 1);
        d.next(&[agent("p1", "working", 3)]);
        d.next(&[agent("p1", "done", 4)]);
        assert_eq!(d.next(&[agent("p1", "done", 4)]).len(), 1);
    }

    #[test]
    fn reads_agents_and_workspace_labels() {
        let snapshot = serde_json::json!({
            "workspaces": [{ "workspace_id": "w1", "label": "legio" }],
            "agents": [{
                "pane_id": "w1:p1", "agent": "claude", "agent_status": "blocked",
                "status_since_unix_ms": 5, "terminal_title_stripped": "fix", "workspace_id": "w1"
            }]
        });
        let agents = parse_agents(&snapshot);
        assert_eq!(
            agents,
            vec![Agent {
                pane_id: "w1:p1".into(),
                agent: "claude".into(),
                status: "blocked".into(),
                since: Some(5),
                title: "fix".into(),
                workspace: "legio".into(),
            }]
        );
        let (title, body, level) = message(&agents[0]);
        assert_eq!(title, "Claude needs input");
        assert_eq!(body, "legio · fix");
        assert_eq!(level, Level::TimeSensitive);
    }
}
