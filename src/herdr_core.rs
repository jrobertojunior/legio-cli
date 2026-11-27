//! herdr-core, the terminal runtime under the Herdr desktop app: what
//! `legio watch` reads from it.
//!
//! The core speaks JSON-RPC one line at a time on the stdin and stdout of
//! `herdr-core connect`. The watcher keeps one `connect` open and asks for
//! `terminal.list` on each poll, so a poll costs no new process.
//!
//! `connect` starts a core when none runs. The watcher does not want that:
//! a person who stopped the core with `hc server stop` meant it. So it
//! reads the core only while the core's socket is there.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::sys;
use crate::watch::Agent;

/// How long one call may take before the connection counts as lost.
const ANSWER: Duration = Duration::from_secs(10);

/// The core's session. `connect` reaches `default` unless `HC_SESSION`
/// says otherwise, and the service sets no `HC_SESSION`.
const SESSION: &str = "default";

/// The `herdr-core` binary: on `PATH`, then where its installer and
/// Homebrew put it. A service starts with a short `PATH`, so the places are
/// looked in by name too.
pub fn find(home: &Path) -> Option<PathBuf> {
    sys::which("herdr-core").or_else(|| {
        [
            home.join(".local/bin/herdr-core"),
            PathBuf::from("/opt/homebrew/bin/herdr-core"),
            PathBuf::from("/usr/local/bin/herdr-core"),
            PathBuf::from("/home/linuxbrew/.linuxbrew/bin/herdr-core"),
        ]
        .into_iter()
        .find(|p| sys::is_executable(p))
    })
}

/// The core's socket, which is there only while the core runs.
fn socket(home: &Path) -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/state"));
    state
        .join("herdr-core")
        .join(SESSION)
        .join(format!("{SESSION}.sock"))
}

pub fn is_running(home: &Path) -> bool {
    socket(home).exists()
}

/// One `herdr-core connect`, and the lines it prints. A thread reads
/// them, so a call can stop waiting when the core does not answer.
struct Connection {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Connection {
    fn open(binary: &Path) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
            .arg("connect")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {} connect", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .context("herdr-core connect has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("herdr-core connect has no stdout")?;
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Connection {
            child,
            stdin,
            lines,
            next_id: 1,
        })
    }

    /// Sends one request and returns its `result`. Lines that answer
    /// something else are skipped.
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").context("herdr-core closed the connection")?;
        self.stdin.flush()?;
        let deadline = Instant::now() + ANSWER;
        loop {
            let line = match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => bail!("herdr-core did not answer {method}"),
                Err(RecvTimeoutError::Disconnected) => bail!("herdr-core closed the connection"),
            };
            let Ok(reply) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                bail!(
                    "{method}: {}",
                    error["message"].as_str().unwrap_or("failed")
                );
            }
            return Ok(reply["result"].clone());
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reads the agents of the core on this machine, over one connection that
/// it opens again after a failure.
pub struct Reader {
    home: PathBuf,
    connection: Option<Connection>,
}

impl Reader {
    pub fn new(home: &Path) -> Self {
        Reader {
            home: home.to_path_buf(),
            connection: None,
        }
    }

    /// The agents now, or `None` when no core runs here.
    pub fn agents(&mut self) -> Option<anyhow::Result<Vec<Agent>>> {
        if !is_running(&self.home) {
            self.connection = None;
            return None;
        }
        if self.connection.is_none() {
            let binary = find(&self.home)?;
            match Connection::open(&binary) {
                Ok(connection) => self.connection = Some(connection),
                Err(e) => return Some(Err(e)),
            }
        }
        let connection = self.connection.as_mut()?;
        let result = connection.call("terminal.list", serde_json::json!({}));
        if result.is_err() {
            self.connection = None;
        }
        Some(result.map(|r| parse_agents(&r)))
    }
}

/// The live terminals that run an agent. The address (`t7`) is the pane
/// id: the app opens a herdr-core terminal by it.
pub fn parse_agents(result: &serde_json::Value) -> Vec<Agent> {
    result["terminals"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["state"] == "live")
        .filter_map(|t| {
            let status = &t["status"];
            let place = t["labels"]["where"].as_str().and_then(split_place);
            Some(Agent {
                pane_id: t["address"].as_str()?.to_string(),
                agent: harness(status["app"].as_str().unwrap_or("agent")).to_string(),
                status: agent_status(t)?.to_string(),
                since: status_since(t),
                title: t["title"].as_str().unwrap_or_default().to_string(),
                workspace: place.unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// The core's word for the program, in Herdr's: the phone and the relay
/// know `claude`.
fn harness(app: &str) -> &str {
    match app {
        "claude-code" => "claude",
        other => other,
    }
}

/// The status in Herdr's words. `error` wants a person, as `blocked` does.
///
/// `done` stays `done` in the core after you read it. The Herdr app writes
/// the last turn you looked at in the `herdr.seen` label, so a turn you
/// already saw on the desk is `idle`, and no push.
fn agent_status(terminal: &serde_json::Value) -> Option<&'static str> {
    match terminal["status"]["state"].as_str()? {
        "idle" => Some("idle"),
        "working" => Some("working"),
        "blocked" | "error" => Some("blocked"),
        "done" => {
            let seen = terminal["labels"]["herdr.seen"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok());
            let turn = terminal["turn"].as_u64();
            match (seen, turn) {
                (Some(seen), Some(turn)) if seen >= turn => Some("idle"),
                _ => Some("done"),
            }
        }
        _ => None,
    }
}

fn status_since(terminal: &serde_json::Value) -> Option<u64> {
    let ms = &terminal["status_ms"];
    ms.as_u64().or_else(|| ms.as_f64().map(|f| f as u64))
}

/// The space part of `caleb / 1`. The last ` / ` splits it from the tab,
/// because a tab is named by its number unless you rename it.
fn split_place(label: &str) -> Option<&str> {
    let (space, tab) = label.rsplit_once(" / ")?;
    (!space.is_empty() && !tab.is_empty()).then_some(space)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_agents_of_live_terminals() {
        let result = serde_json::json!({"terminals": [
            {"address": "t1", "state": "live", "title": "Fastlane config",
             "labels": {"where": "caleb / 1", "herdr.seen": "11"}, "turn": 12,
             "status": {"app": "claude-code", "state": "done"}, "status_ms": 1794793073641u64},
            {"address": "t2", "state": "live", "title": "", "labels": {"herdr.seen": "4"}, "turn": 4,
             "status": {"app": "claude-code", "state": "done"}, "status_ms": 7},
            {"address": "t3", "state": "live", "title": null, "labels": {},
             "status": {"app": "codex", "state": "error"}, "status_ms": null},
            {"address": "t4", "state": "live", "labels": {}, "status": null},
            {"address": "t5", "state": "finished", "labels": {},
             "status": {"app": "codex", "state": "blocked"}}
        ]});
        let agents = parse_agents(&result);
        assert_eq!(
            agents[0],
            Agent {
                pane_id: "t1".into(),
                agent: "claude".into(),
                status: "done".into(),
                since: Some(1794793073641),
                title: "Fastlane config".into(),
                workspace: "caleb".into(),
            }
        );
        // Seen on the desk: no news.
        assert_eq!(agents[1].status, "idle");
        assert_eq!(agents[2].status, "blocked");
        assert_eq!(agents[2].agent, "codex");
        assert_eq!(agents[2].since, None);
        assert_eq!(agents.len(), 3);
    }

    #[test]
    fn splits_the_place_at_the_last_slash() {
        assert_eq!(split_place("a / b / 2"), Some("a / b"));
        assert_eq!(split_place("no tab"), None);
    }
}
