//! Running other programs, and the facts about this machine and user that
//! every step needs.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

/// Runs a program and returns its trimmed stdout, or `None` when it is
/// missing, fails, or prints nothing. For questions — "is lingering on?"
/// — where a failure means "no answer", not "stop".
pub fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Runs a program with the terminal attached, so `sudo` can ask for a
/// password and a package manager can show its progress.
pub fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("could not start {program}"))?;
    if !status.success() {
        bail!("{program} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Runs a program and reports only whether it succeeded, with its output
/// thrown away.
pub fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The full path of a program on `PATH`, like `command -v`.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

pub fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// The login the phone will use. `USER` first, because that is what the
/// bash script used and what a person running this expects; `id -un` for
/// the shells that do not set it.
pub fn user() -> anyhow::Result<String> {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| output("id", &["-un"]))
        .context("could not tell which user this is")
}

pub fn hostname() -> String {
    output("hostname", &[]).unwrap_or_else(|| "server".to_string())
}

/// Installs one package with whichever package manager this machine has.
/// `apk` is the name Alpine uses, because the managers do not agree —
/// Alpine calls `qrencode` `libqrencode-tools`. Every other manager takes
/// `apt`.
pub fn install_package(apt: &str, apk: &str) -> anyhow::Result<()> {
    if which("apt-get").is_some() {
        run("sudo", &["apt-get", "update"])?;
        run("sudo", &["apt-get", "install", "-y", apt])
    } else if which("dnf").is_some() {
        run("sudo", &["dnf", "install", "-y", apt])
    } else if which("yum").is_some() {
        run("sudo", &["yum", "install", "-y", apt])
    } else if which("pacman").is_some() {
        run("sudo", &["pacman", "-Sy", "--noconfirm", apt])
    } else if which("apk").is_some() {
        run("sudo", &["apk", "add", apk])
    } else {
        bail!("no known package manager. Install {apt} by hand, then run this again")
    }
}

/// Fills a buffer from the kernel's random source. Read directly rather
/// than through a crate, because this tool needs 32 random bytes once per
/// run and nothing else.
pub fn random_bytes<const N: usize>() -> anyhow::Result<[u8; N]> {
    use std::io::Read;
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read /dev/urandom")?;
    Ok(bytes)
}
