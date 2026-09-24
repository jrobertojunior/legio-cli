//! The name a person compares: five words taken from the public key.
//!
//! A `SHA256:` fingerprint is 43 characters of base64 that nobody reads
//! aloud or checks past the first few. Five words are read whole, and the
//! phone and this tool show the same five.
//!
//! The words come from the SHA-256 of the key's blob — the same digest the
//! fingerprint is made from — so the phrase names the key and nothing else.
//! The list has 2048 words, so a word carries exactly 11 bits and five words
//! carry 55. Someone who wants to pass a key of their own off as the phone's
//! must find one whose digest starts with the same 55 bits, about 3.6e16
//! tries, inside the ten minutes the pairing code lives.
//!
//! The list is the BIP-39 English list: 2048 common words, none of them
//! near-spellings of another, and every word is told apart by its first four
//! letters. The app carries the same list; `wordlist.txt` here and
//! `KeyPhrase.swift` there must never differ, or the two screens disagree.
//! The test below pins the list, and the phrase of one known key, so a
//! change on either side fails a test.

use sha2::{Digest, Sha256};

const WORDS: &str = include_str!("wordlist.txt");
pub const WORD_COUNT: usize = 5;
const BITS_PER_WORD: usize = 11;

/// The phrase for a digest: the first 55 bits, eleven at a time, as
/// indexes into the list. Bit order is big-endian, so the first word is
/// made of the digest's first bits.
pub fn from_digest(digest: &[u8]) -> String {
    let head = u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("a SHA-256 digest is 32 bytes"),
    );
    let bits = head >> (64 - WORD_COUNT * BITS_PER_WORD);
    let words: Vec<&str> = WORDS.lines().collect();
    (0..WORD_COUNT)
        .map(|i| {
            let shift = BITS_PER_WORD * (WORD_COUNT - 1 - i);
            words[((bits >> shift) & 0x7ff) as usize]
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// The phrase for a key blob, as the bytes the base64 in `authorized_keys`
/// decodes to.
pub fn of_blob(blob: &[u8]) -> String {
    from_digest(&Sha256::digest(blob))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    #[test]
    fn the_list_is_the_one_the_app_carries() {
        assert_eq!(WORDS.lines().count(), 2048);
        // SHA-256 of wordlist.txt. The same value is pinned in the app's
        // KeyPhrase.swift notes; change both or neither.
        let digest = Sha256::digest(WORDS.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda"
        );
    }

    /// The phrase for the key `ssh-keygen` made in the app's generator
    /// test, worked out separately in Python. The app's own check uses the
    /// same digest and the same phrase.
    #[test]
    fn a_known_key_has_a_known_phrase() {
        let blob = STANDARD
            .decode("AAAAC3NzaC1lZDI1NTE5AAAAIHI2iP/D59jopcwuQ7odefdufWyYlto1QwkLRcmzaf87")
            .unwrap();
        assert_eq!(of_blob(&blob), "fee-jazz-naive-fruit-equip");
    }

    #[test]
    fn the_extremes_of_the_digest_reach_the_ends_of_the_list() {
        let mut digest = [0u8; 32];
        assert_eq!(
            from_digest(&digest),
            "abandon-abandon-abandon-abandon-abandon"
        );
        digest[..8].fill(0xff);
        assert_eq!(from_digest(&digest), "zoo-zoo-zoo-zoo-zoo");
    }
}
