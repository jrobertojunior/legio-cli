//! Reading and changing `~/.ssh/authorized_keys` without harming the keys
//! that are not ours.
//!
//! Everything careful here exists because an earlier setup script ate a
//! working key. It appended with `>>` and no newline
//! guard, so on a file whose last line had no newline the new key landed on
//! the end of the old one and joined them into one unusable entry. The
//! uninstall then matched its own key anywhere in the line and deleted the
//! whole line, taking the other key with it.
//!
//! So: the file is rewritten whole, from its own lines, every line we do
//! not own kept byte for byte. A line holding more than one key is damage
//! from that bug and is never touched. The old file is copied aside first,
//! and the new one replaces it in one rename.
//!
//! Our entries are the ones whose comment starts with `legio-app:`. The
//! file itself is the list of paired phones — there is no second record to
//! drift away from it.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::sshkey::{self, PublicKey};

/// What every comment this tool writes starts with. The phone's name
/// follows it.
pub const MARKER: &str = "legio-app:";

/// One phone's entry, as found in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub blob: String,
    pub options: String,
}

impl Device {
    pub fn phrase(&self) -> String {
        sshkey::phrase_of_blob(&self.blob).unwrap_or_else(|_| "(unreadable key)".into())
    }
}

/// One line, understood only as far as this tool needs.
#[derive(Debug)]
enum Line<'a> {
    /// One key. `options` is empty for an entry without any.
    Key {
        options: &'a str,
        kind: &'a str,
        blob: &'a str,
        comment: &'a str,
    },
    /// A blank line, a `#` comment, or anything not recognised. Kept as is.
    Other,
    /// More than one key on one line. Kept as is, and reported.
    Joined,
}

fn is_key_type(token: &str) -> bool {
    token.starts_with("ssh-") || token.starts_with("ecdsa-") || token.starts_with("sk-")
}

/// A key type glued to the end of another word — `me@laptopssh-ed25519` —
/// is how that bug joined two entries that had no options. Counting only
/// tokens that *start* with a key type misses this shape.
fn holds_key_type(token: &str) -> bool {
    is_key_type(token)
        || [
            "ssh-ed25519",
            "ssh-rsa",
            "ssh-dss",
            "ecdsa-sha2-",
            "sk-ssh-",
            "sk-ecdsa-",
        ]
        .iter()
        .any(|kind| token.contains(kind))
}

/// Splits a line into whitespace-separated tokens, keeping `"..."` in one
/// piece: the options field can hold `permitopen="host:port"` and
/// `command="a b"`, and a space inside the quotes is not a separator.
/// Returns each token with its byte offset, so the comment can be cut from
/// the original line with its spacing intact.
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == bytes.len() {
            break;
        }
        let start = i;
        let mut quoted = false;
        while i < bytes.len() && (quoted || !bytes[i].is_ascii_whitespace()) {
            match bytes[i] {
                b'\\' if quoted => i += 1,
                b'"' => quoted = !quoted,
                _ => {}
            }
            i += 1;
        }
        let end = i.min(bytes.len());
        out.push((start, &line[start..end]));
    }
    out
}

fn classify(line: &str) -> Line<'_> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Line::Other;
    }
    let toks = tokens(line);
    if toks.iter().filter(|(_, t)| holds_key_type(t)).count() > 1 {
        return Line::Joined;
    }
    // `<type> <blob> [comment]`, or `<options> <type> <blob> [comment]`.
    let at = if is_key_type(toks[0].1) { 0 } else { 1 };
    let (Some(&(_, kind)), Some(&(blob_start, blob))) = (toks.get(at), toks.get(at + 1)) else {
        return Line::Other;
    };
    if !is_key_type(kind) {
        return Line::Other;
    }
    let options = if at == 1 { toks[0].1 } else { "" };
    let comment = line[blob_start + blob.len()..].trim();
    Line::Key {
        options,
        kind,
        blob,
        comment,
    }
}

/// Every phone this tool paired, in file order.
pub fn devices(text: &str) -> Vec<Device> {
    text.lines()
        .filter_map(|line| match classify(line) {
            Line::Key {
                options,
                blob,
                comment,
                ..
            } => comment.strip_prefix(MARKER).map(|name| Device {
                name: name.to_string(),
                blob: blob.into(),
                options: options.into(),
            }),
            _ => None,
        })
        .collect()
}

/// How many lines hold more than one key. Worth a warning, never an edit.
pub fn joined_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| matches!(classify(l), Line::Joined))
        .count()
}

/// Makes a phone's name safe to be the last word of an `authorized_keys`
/// line: letters, digits, `.`, `_`, `-`, and at most 40 of them. The name
/// arrives from the network, and a space or a newline in it would change
/// what sshd reads.
pub fn device_slug(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "phone".into()
    } else {
        slug
    }
}

fn entry(options: &str, kind: &str, blob: &str, comment: &str) -> String {
    let mut line = String::new();
    if !options.is_empty() {
        line.push_str(options);
        line.push(' ');
    }
    line.push_str(kind);
    line.push(' ');
    line.push_str(blob);
    if !comment.is_empty() {
        line.push(' ');
        line.push_str(comment);
    }
    line
}

/// Joins lines back into a file that ends in exactly one newline — the
/// guard the old script was missing.
fn join(lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// The file with this phone's key added.
///
/// An entry already carrying the same name is replaced, not kept beside
/// the new one: re-pairing a phone means its old key is no longer on it,
/// and a key nobody holds is a key that should not open anything. An
/// entry with the same key is replaced too, so pairing twice never leaves
/// two lines.
pub fn with_device(
    text: &str,
    key: &PublicKey,
    name: &str,
    options: &str,
) -> (String, Vec<Device>) {
    let comment = format!("{MARKER}{name}");
    let mut replaced = Vec::new();
    let mut lines: Vec<String> = text
        .lines()
        .filter(|line| match classify(line) {
            Line::Key {
                options,
                blob,
                comment: c,
                ..
            } if c == comment || blob == key.blob => {
                if let Some(device_name) = c.strip_prefix(MARKER) {
                    replaced.push(Device {
                        name: device_name.into(),
                        blob: blob.into(),
                        options: options.into(),
                    });
                }
                false
            }
            _ => true,
        })
        .map(str::to_string)
        .collect();
    lines.push(entry(options, sshkey::ED25519, &key.blob, &comment));
    (join(lines), replaced)
}

/// The file with every paired phone's options set to `options`. Returns
/// how many entries changed. Used when `--forward-port` changes what a key
/// may do on a machine that is already paired.
pub fn with_options(text: &str, options: &str) -> (String, usize) {
    let mut changed = 0;
    let lines = text
        .lines()
        .map(|line| match classify(line) {
            Line::Key {
                options: old,
                kind,
                blob,
                comment,
            } if comment.starts_with(MARKER) && old != options => {
                changed += 1;
                entry(options, kind, blob, comment)
            }
            _ => line.to_string(),
        })
        .collect();
    (join(lines), changed)
}

/// The file without the entries `remove` picks. Returns what was removed.
/// Only single-key lines are ever candidates — see the module note.
pub fn without(text: &str, mut remove: impl FnMut(&str, &str) -> bool) -> (String, usize) {
    let mut removed = 0;
    let lines = text
        .lines()
        .filter(|line| match classify(line) {
            Line::Key { blob, comment, .. } if remove(blob, comment) => {
                removed += 1;
                false
            }
            _ => true,
        })
        .map(str::to_string)
        .collect();
    (join(lines), removed)
}

/// The file on disk, and the one safe way to change it.
pub struct File {
    pub path: PathBuf,
}

impl File {
    pub fn in_home(home: &Path) -> Self {
        Self {
            path: home.join(".ssh/authorized_keys"),
        }
    }

    pub fn read(&self) -> anyhow::Result<String> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", self.path.display())),
        }
    }

    /// Replaces the file with `text`. Copies the old file aside first, and
    /// refuses to go on without that copy: sshd reads only
    /// `authorized_keys` itself, so a copy beside it costs nothing, and it
    /// is the difference between a bad run being an annoyance and being a
    /// locked door. Returns the path of the copy, if there was anything to
    /// copy.
    pub fn write(&self, text: &str) -> anyhow::Result<Option<PathBuf>> {
        let dir = self
            .path
            .parent()
            .context("authorized_keys has no parent directory")?;
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

        let backup = if fs::metadata(&self.path).is_ok_and(|m| m.len() > 0) {
            let stamp =
                crate::sys::output("date", &["+%Y%m%d-%H%M%S"]).unwrap_or_else(|| "backup".into());
            let backup = self
                .path
                .with_file_name(format!("authorized_keys.herdr-backup-{stamp}"));
            fs::copy(&self.path, &backup).with_context(|| {
                format!(
                    "could not back up {}; stopping before it changes",
                    self.path.display()
                )
            })?;
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))?;
            Some(backup)
        } else {
            None
        };

        // Write beside it and rename over it, so a full disk or a kill
        // halfway leaves the old file, never half of the new one.
        let tmp = self.path.with_file_name("authorized_keys.herdr-tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("could not write {}", tmp.display()))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        Ok(backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::tests::sample_line;

    const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOther me@laptop";

    fn key(seed: u8) -> PublicKey {
        PublicKey::parse(&sample_line(seed)).unwrap()
    }

    #[test]
    fn adds_a_line_and_mends_a_missing_newline() {
        // No newline at the end: the case that joined two keys before.
        let (text, replaced) = with_device(OTHER, &key(1), "iPhone", "restrict,pty");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines[0], OTHER);
        assert!(lines[1].starts_with("restrict,pty ssh-ed25519 "));
        assert!(lines[1].ends_with(" legio-app:iPhone"));
        assert!(text.ends_with('\n'));
        assert!(replaced.is_empty());
    }

    #[test]
    fn re_pairing_a_phone_replaces_its_old_key() {
        let (text, _) = with_device(OTHER, &key(1), "iPhone", "restrict");
        let (text, replaced) = with_device(&text, &key(2), "iPhone", "restrict");
        assert_eq!(devices(&text).len(), 1);
        assert_eq!(devices(&text)[0].blob, key(2).blob);
        assert_eq!(replaced.len(), 1);
        assert!(text.contains(OTHER));
    }

    #[test]
    fn options_with_quoted_spaces_stay_one_field() {
        let line = format!(
            r#"command="echo a b",permitopen="127.0.0.1:1" {} legio-app:x"#,
            key(3).line()
        );
        let found = devices(&line);
        assert_eq!(
            found[0].options,
            r#"command="echo a b",permitopen="127.0.0.1:1""#
        );
        assert_eq!(found[0].name, "x");
    }

    #[test]
    fn rewrites_options_on_our_lines_only() {
        let (text, _) = with_device(&format!("restrict {OTHER}\n"), &key(1), "a", "restrict,pty");
        let (text, changed) = with_options(&text, "restrict,pty,port-forwarding");
        assert_eq!(changed, 1);
        assert!(text.starts_with(&format!("restrict {OTHER}\n")));
        assert_eq!(devices(&text)[0].options, "restrict,pty,port-forwarding");
    }

    #[test]
    fn never_touches_a_joined_line() {
        // Both shapes the old bug made: the second entry had options, so a
        // space still split it off; or it had none, and its key type was
        // glued to the first entry's comment.
        for joined in [
            format!("{OTHER}restrict {} herdr-poc@vps\n", key(1).line()),
            format!("{OTHER}{} herdr-poc@vps\n", key(1).line()),
        ] {
            assert_eq!(joined_lines(&joined), 1, "{joined}");
            let (text, removed) = without(&joined, |_, _| true);
            assert_eq!(removed, 0);
            assert_eq!(text, joined);
        }
    }

    #[test]
    fn keeps_comments_and_blank_lines() {
        let original = format!("# mine\n\n{OTHER}\n");
        let (text, _) = with_device(&original, &key(1), "a", "");
        assert!(text.starts_with(&original));
        let (text, removed) = without(&text, |_, c| c.starts_with(MARKER));
        assert_eq!(removed, 1);
        assert_eq!(text, original);
    }

    #[test]
    fn slugs_are_one_safe_word() {
        assert_eq!(device_slug("João's iPhone 15\n"), "Jo-o-s-iPhone-15");
        assert_eq!(device_slug("  "), "phone");
    }
}
