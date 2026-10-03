//! `legio update`: replaces this binary with a release from GitHub.
//!
//! It downloads the same tarball `install.sh` does, checks it against the
//! release's `SHA256SUMS`, runs the new binary once, and renames it over
//! this one. A rename, not a write into the file: a process that runs the
//! old binary keeps its file, and macOS does not kill the new one for a
//! code signature that changed under it.
//!
//! Then it runs `<new binary> after-update`, so each release does its own
//! work after an update — restart a service it changed, for example —
//! without the old binary knowing about it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

use crate::ui::{info, warn};

pub const REPO: &str = "jrobertojunior/legio-cli";

/// The version this binary is, from Cargo.toml.
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The first release that knows `after-update`. An older one, installed to
/// go back from a bad release, is not asked to run it.
const FIRST_WITH_AFTER_UPDATE: (u64, u64, u64) = (0, 1, 1);

/// The release asset for this machine. The same names `install.sh` builds
/// from `uname -s` and `uname -m`.
pub fn asset_name() -> anyhow::Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "legio-Darwin-arm64.tar.gz",
        ("macos", "x86_64") => "legio-Darwin-x86_64.tar.gz",
        ("linux", "x86_64") => "legio-Linux-x86_64.tar.gz",
        ("linux", "aarch64") => "legio-Linux-aarch64.tar.gz",
        (os, arch) => bail!("there is no release build for {os} {arch}"),
    })
}

/// `v1.2.3` or `1.2.3`, as three numbers. Anything after a `-` is ignored.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v').split('-').next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .into()
}

/// The tag of the latest release.
fn latest_tag(agent: &ureq::Agent) -> anyhow::Result<String> {
    let release: serde_json::Value = agent
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("accept", "application/vnd.github+json")
        .call()
        .context("could not ask GitHub for the latest release")?
        .body_mut()
        .read_json()
        .context("GitHub sent a release that is not JSON")?;
    release["tag_name"]
        .as_str()
        .map(str::to_string)
        .context("the latest release has no tag")
}

fn download(agent: &ureq::Agent, url: &str) -> anyhow::Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("could not download {url}"))?
        .body_mut()
        .with_config()
        .limit(100 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("could not download {url}"))
}

/// The SHA-256 that `SHA256SUMS` gives for one file, in lowercase hex.
pub fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (sum, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset).then(|| sum.to_ascii_lowercase())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The file to replace: this binary, with symlinks followed, so a link in
/// `~/.local/bin` to the real file updates the real file.
fn target() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("could not tell where this legio binary is")?;
    std::fs::canonicalize(&exe).with_context(|| format!("could not resolve {}", exe.display()))
}

pub fn run(version: Option<&str>, check_only: bool, force: bool) -> anyhow::Result<()> {
    let agent = agent();
    let asset = asset_name()?;
    let tag = match version {
        Some(v) => {
            parse_version(v).with_context(|| format!("{v} is not a version like v0.2.0"))?;
            format!("v{}", v.trim_start_matches('v'))
        }
        None => latest_tag(&agent)?,
    };
    let current = parse_version(CURRENT).context("this binary has no version")?;
    let wanted =
        parse_version(&tag).with_context(|| format!("the release tag {tag} is not a version"))?;

    info(&format!("This is legio v{CURRENT}. The release is {tag}."));
    if check_only {
        if wanted > current {
            info("An update is ready. Install it with: legio update");
        } else {
            info("This is the latest version.");
        }
        return Ok(());
    }
    // A version asked for by name is installed even when it is older: that
    // is how you go back from a bad release.
    if version.is_none() && wanted <= current && !force {
        info("This is the latest version. Nothing to do.");
        return Ok(());
    }

    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    info(&format!("Downloading {asset} …"));
    let tarball = download(&agent, &format!("{base}/{asset}"))?;
    let sums = String::from_utf8(download(&agent, &format!("{base}/SHA256SUMS"))?)
        .context("SHA256SUMS is not text")?;
    let expected = expected_sum(&sums, asset)
        .with_context(|| format!("SHA256SUMS has no line for {asset}"))?;
    let actual = sha256_hex(&tarball);
    if actual != expected {
        bail!(
            "{asset} does not match SHA256SUMS (got {actual}, want {expected}). Nothing was changed"
        );
    }
    info("The checksum matches SHA256SUMS.");

    let target = target()?;
    let dir = target.parent().context("the binary has no directory")?;
    let staged = stage(&tarball, dir)?;
    let result = check_and_swap(&staged, &target);
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result?;
    info(&format!("Installed legio {tag} at {}.", target.display()));

    // The new binary finishes the job: it knows what changed, this one does not.
    if wanted < FIRST_WITH_AFTER_UPDATE {
        return Ok(());
    }
    let status = Command::new(&target)
        .arg("after-update")
        .arg("--from")
        .arg(CURRENT)
        .status();
    if !status.is_ok_and(|s| s.success()) {
        warn("The new version could not finish the update. Run: legio setup");
    }
    Ok(())
}

/// Unpacks the tarball next to the binary, so the last step is a rename on
/// one file system.
fn stage(tarball: &[u8], dir: &Path) -> anyhow::Result<PathBuf> {
    let work = dir.join(format!(".legio-update-{}", std::process::id()));
    std::fs::create_dir_all(&work).with_context(|| {
        format!(
            "could not write in {}. Run the update as the user that owns it, or reinstall with install.sh",
            dir.display()
        )
    })?;
    let unpacked = (|| {
        let archive = work.join("legio.tar.gz");
        std::fs::write(&archive, tarball)?;
        let ok = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&work)
            .status()
            .context("could not start tar")?
            .success();
        if !ok {
            bail!("tar could not unpack the release");
        }
        let staged = dir.join(format!(".legio-new-{}", std::process::id()));
        std::fs::rename(work.join("legio"), &staged).context("the release has no legio binary")?;
        Ok(staged)
    })();
    let _ = std::fs::remove_dir_all(&work);
    unpacked
}

/// Runs the new binary once, so a build that cannot start on this machine
/// never takes the old one's place. Then renames it over the old one.
fn check_and_swap(staged: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))?;
    let output = Command::new(staged)
        .arg("--version")
        .output()
        .context("the new binary does not start on this machine. Nothing was changed")?;
    if !output.status.success() {
        bail!("the new binary does not start on this machine. Nothing was changed");
    }
    std::fs::rename(staged, target)
        .with_context(|| format!("could not replace {}", target.display()))
}

/// What this version does after an update installed it. The binary that
/// ran the update does not know what this one changed, so this one does it.
pub fn after_update(from: &str) -> anyhow::Result<()> {
    if from != CURRENT {
        info(&format!("Updated from v{from} to v{CURRENT}."));
    }
    let home = crate::sys::home()?;
    if crate::watch::is_installed(&home) {
        // The watcher runs this binary, and still runs the old one.
        crate::watch::restart(&home)?;
        info("Restarted the notification watcher.");
    } else if bridge_installed(&home) {
        // 0.2.0 brought the watcher. A machine set up before it gets it
        // here, so the update alone turns notifications on.
        info("Installing the notification watcher.");
        crate::watch::install(&home, &home.join(".config/herdr/herdr.sock"))?;
    } else {
        info("This machine has no bridge. Run legio setup to use it with the app.");
    }
    Ok(())
}

/// Whether `legio setup` ran on this machine.
fn bridge_installed(home: &Path) -> bool {
    if cfg!(target_os = "macos") {
        crate::mac::agent_plist(home).exists()
    } else {
        crate::bridge::installed_unit(home).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v0.2.10"), Some((0, 2, 10)));
        assert_eq!(parse_version("1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_version("v1.0.0-rc1"), Some((1, 0, 0)));
        assert_eq!(parse_version("v1.0"), None);
        assert_eq!(parse_version("v1.0.0.1"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("v0.10.0") > parse_version("v0.9.9"));
    }

    #[test]
    fn finds_the_sum_for_one_asset() {
        let sums = "abc123  legio-Darwin-arm64.tar.gz\nDEF456 *legio-Linux-x86_64.tar.gz\n";
        assert_eq!(
            expected_sum(sums, "legio-Darwin-arm64.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            expected_sum(sums, "legio-Linux-x86_64.tar.gz").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_sum(sums, "legio-Linux-aarch64.tar.gz"), None);
    }

    #[test]
    fn this_machine_has_an_asset() {
        assert!(asset_name().is_ok());
    }
}
