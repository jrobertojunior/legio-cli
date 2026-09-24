//! Reading the one public key type the app makes, and naming keys the way
//! `ssh-keygen -l` does.

use anyhow::{Context, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use sha2::{Digest, Sha256};

pub const ED25519: &str = "ssh-ed25519";

/// A public key the phone sent: the type, the base64 blob, and nothing
/// else. The comment the phone sent is dropped — see `authorized_keys`
/// for the comment this tool writes instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub blob: String,
}

impl PublicKey {
    /// Reads `ssh-ed25519 <base64> [comment]`.
    ///
    /// Only ed25519, because that is the only key the app makes. The blob
    /// is decoded and checked field by field rather than taken on trust:
    /// this line is about to go into `authorized_keys`, and a line sshd
    /// cannot read makes it skip the entry without a word.
    pub fn parse(line: &str) -> anyhow::Result<Self> {
        let mut fields = line.split_whitespace();
        let kind = fields.next().context("the public key is empty")?;
        if kind != ED25519 {
            bail!("the key is {kind}; only {ED25519} keys are accepted");
        }
        let blob = fields.next().context("the public key has no key data")?;
        let bytes = STANDARD
            .decode(blob)
            .context("the key data is not base64")?;

        // The blob is an SSH `string` naming the type, then a `string`
        // holding the 32-byte point.
        let (name, rest) = ssh_string(&bytes)?;
        ensure!(
            name == ED25519.as_bytes(),
            "the key data names another key type"
        );
        let (point, rest) = ssh_string(rest)?;
        ensure!(
            point.len() == 32,
            "an ed25519 key is 32 bytes, not {}",
            point.len()
        );
        ensure!(rest.is_empty(), "the key data has bytes after the key");

        Ok(Self {
            blob: blob.to_string(),
        })
    }

    /// The five words a person compares — see `phrase`.
    pub fn phrase(&self) -> String {
        phrase_of_blob(&self.blob).expect("the blob was decoded in `parse`")
    }

    #[cfg(test)]
    pub fn line(&self) -> String {
        format!("{ED25519} {}", self.blob)
    }
}

/// The words for a key blob given as the base64 in `authorized_keys`.
pub fn phrase_of_blob(blob: &str) -> anyhow::Result<String> {
    let bytes = STANDARD
        .decode(blob)
        .context("the key data is not base64")?;
    Ok(crate::phrase::of_blob(&bytes))
}

/// `SHA256:<base64 without padding>` of the decoded blob — the form
/// `ssh-keygen -l` prints, so a person can compare it with what they see
/// anywhere else.
pub fn fingerprint_of_blob(blob: &str) -> anyhow::Result<String> {
    let bytes = STANDARD
        .decode(blob)
        .context("the key data is not base64")?;
    Ok(format!(
        "SHA256:{}",
        STANDARD_NO_PAD.encode(Sha256::digest(&bytes))
    ))
}

/// Splits one SSH `string` — a big-endian length, then that many bytes —
/// off the front of `bytes`.
fn ssh_string(bytes: &[u8]) -> anyhow::Result<(&[u8], &[u8])> {
    ensure!(bytes.len() >= 4, "the key data ends early");
    let len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let rest = &bytes[4..];
    ensure!(rest.len() >= len, "the key data ends early");
    Ok(rest.split_at(len))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A public key line for a made-up point. Nothing here signs, so the
    /// point does not have to be on the curve.
    pub fn sample_line(seed: u8) -> String {
        let mut blob = Vec::new();
        for field in [ED25519.as_bytes(), &[seed; 32][..]] {
            blob.extend((field.len() as u32).to_be_bytes());
            blob.extend(field);
        }
        format!("{ED25519} {} phone", STANDARD.encode(blob))
    }

    #[test]
    fn reads_an_ed25519_line_and_drops_the_comment() {
        let key = PublicKey::parse(&sample_line(7)).unwrap();
        assert!(
            key.line()
                .starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI")
        );
        assert!(!key.line().contains("phone"));
    }

    /// The key `ssh-keygen` made for the app's generator test, with the
    /// fingerprint `ssh-keygen -l` printed for it.
    #[test]
    fn fingerprint_matches_ssh_keygen() {
        let key = PublicKey::parse(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHI2iP/D59jopcwuQ7odefdufWyYlto1QwkLRcmzaf87 test",
        )
        .unwrap();
        assert_eq!(
            fingerprint_of_blob(&key.blob).unwrap(),
            "SHA256:VI7uSq7kwiejD5+QcXWkgL/Itf9NM2bARLQW/VTm3m4"
        );
        assert_eq!(key.phrase(), "fee-jazz-naive-fruit-equip");
    }

    #[test]
    fn refuses_other_types_and_damaged_blobs() {
        assert!(PublicKey::parse("ssh-rsa AAAAB3NzaC1yc2E= x").is_err());
        assert!(PublicKey::parse("ssh-ed25519 !!!").is_err());
        // Right type, blob cut short.
        let line = sample_line(1);
        let short = &line[..line.find(' ').unwrap() + 30];
        assert!(PublicKey::parse(short).is_err());
    }
}
