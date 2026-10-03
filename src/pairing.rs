//! The pairing exchange: a QR code out to the phone, a public key back.
//!
//! The phone makes its own key, and only the public half ever leaves it.
//! A key made here and handed over in the QR code would live on the
//! server, on the screen, and in any photo of the screen.
//!
//! 1. This tool prints a QR code with the connection details, the server's
//!    host key fingerprint, a port, and a one-time token. Nothing in it is
//!    a login.
//! 2. The phone scans it, makes an ed25519 key, and POSTs the public key to
//!    `http://<host>:<pairPort>/v1/pair`, with an HMAC of it keyed by the
//!    token.
//! 3. This tool checks the HMAC, shows the key's five-word phrase and the
//!    phone's name, and asks the person at the terminal. Only a yes writes
//!    `authorized_keys`.
//!
//! Plain HTTP is enough: a public key is not a secret, and the HMAC is what
//! stops someone else's key being slipped in. The token is the proof that
//! whoever sends a key has seen the QR code, and the question at the
//! terminal covers the case where someone else has seen it too. See
//! `PROTOCOL.md` for the wire format.
//!
//! The same request can carry the phone's push secret, so a pairing also
//! turns on notifications. That one *is* a secret, so it travels sealed
//! with a key made from the token.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, Payload as Sealed};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::authorized_keys::device_slug;
use crate::sshkey::PublicKey;

pub const PAYLOAD_VERSION: u32 = 1;
pub const PATH: &str = "/v1/pair";
/// A bad HMAC is either a bug in the app or someone guessing. Past this
/// many, stop: the token has 256 bits and nobody guesses it, so a stream
/// of bad ones is not a phone.
const MAX_BAD_ATTEMPTS: u32 = 5;
/// A public key line and a phone name fit in far less than this. Anything
/// bigger is not a pairing request.
const MAX_BODY: u64 = 16 * 1024;

/// What the QR code holds. Short field names, because a longer payload is
/// a denser code that a phone camera reads less well.
#[derive(Debug, Serialize)]
pub struct Payload {
    pub v: u32,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(rename = "targetPort")]
    pub target_port: u16,
    pub session: String,
    /// The fingerprint of the server's ed25519 host key, so the app can
    /// check it is talking to this machine and not to one in the middle.
    /// Absent when the host key cannot be read.
    #[serde(rename = "hostKey", skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    #[serde(rename = "pairPort")]
    pub pair_port: u16,
    pub token: String,
}

/// A fresh one-time token: 32 random bytes, base64url without padding, so
/// it sits in JSON and in a QR code with no escaping. The HMAC key is the
/// token's own UTF-8 bytes — the phone does not have to decode it.
pub fn new_token() -> anyhow::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(crate::sys::random_bytes::<32>()?))
}

/// What the phone POSTs.
#[derive(Debug, Deserialize, Serialize)]
pub struct PairRequest {
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub device: String,
    pub mac: String,
    /// The phone's push secret, when it allows notifications. Absent from
    /// an app that does not, and from apps older than this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push: Option<PushRequest>,
}

/// The push part of a pairing request: the relay's device secret for the
/// phone, sealed, and the app's id for its connection to this machine.
#[derive(Debug, Deserialize, Serialize)]
pub struct PushRequest {
    #[serde(rename = "connectionId")]
    pub connection_id: String,
    /// Standard base64 of ChaCha20-Poly1305's nonce (12 bytes), ciphertext
    /// and tag (16 bytes), in that order — CryptoKit's `combined`.
    pub sealed: String,
}

/// The push secret, opened.
#[derive(Debug, PartialEq, Eq)]
pub struct PushGrant {
    pub connection_id: String,
    pub secret: String,
}

/// The sealing key: HMAC-SHA256 of a fixed label, keyed by the token. Not
/// the token itself, so the HMAC on the request and the seal never share
/// a key.
fn push_key(token: &str) -> [u8; 32] {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    mac.update(b"legio-push-v1");
    mac.finalize().into_bytes().into()
}

/// The bytes the seal covers besides the secret, so a sealed secret cannot
/// be moved to another connection id.
fn push_aad(connection_id: &str) -> String {
    format!("legio-push-v1\n{connection_id}")
}

impl PushRequest {
    pub fn open(&self, token: &str) -> anyhow::Result<PushGrant> {
        let id_ok = (1..=100).contains(&self.connection_id.len())
            && self
                .connection_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !id_ok {
            bail!("the phone sent a connection id this tool does not accept");
        }
        let sealed = STANDARD
            .decode(&self.sealed)
            .context("the push secret is not base64")?;
        if sealed.len() < 12 + 16 {
            bail!("the push secret is too short");
        }
        let (nonce, sealed) = sealed.split_at(12);
        let key = Key::from(push_key(token));
        let nonce =
            Nonce::try_from(nonce).map_err(|_| anyhow!("the push nonce is not 12 bytes"))?;
        let aad = push_aad(&self.connection_id);
        let secret = ChaCha20Poly1305::new(&key)
            .decrypt(
                &nonce,
                Sealed {
                    msg: sealed,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("the push secret was not sealed with this pairing code"))?;
        let secret = String::from_utf8(secret).context("the push secret is not text")?;
        crate::push::check_secret(&secret)?;
        Ok(PushGrant {
            connection_id: self.connection_id.clone(),
            secret,
        })
    }

    /// What the app does. Here for the tests, which play the phone.
    #[cfg(test)]
    pub fn seal(token: &str, connection_id: &str, secret: &str) -> Self {
        let key = Key::from(push_key(token));
        let nonce = [7u8; 12];
        let aad = push_aad(connection_id);
        let mut combined = nonce.to_vec();
        combined.extend(
            ChaCha20Poly1305::new(&key)
                .encrypt(
                    &Nonce::from(nonce),
                    Sealed {
                        msg: secret.as_bytes(),
                        aad: aad.as_bytes(),
                    },
                )
                .unwrap(),
        );
        Self {
            connection_id: connection_id.into(),
            sealed: STANDARD.encode(combined),
        }
    }
}

/// The bytes the HMAC covers. The public key *and* the name, so neither
/// can be swapped on the way. The prefix keeps this MAC from being valid
/// for any other message a later version signs with the same token.
pub fn mac_message(public_key: &str, device: &str) -> String {
    format!("herdr-pair-v1\n{public_key}\n{device}")
}

/// What the app does. Here for the tests, which play the phone.
#[cfg(test)]
pub fn sign(token: &str, public_key: &str, device: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    mac.update(mac_message(public_key, device).as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// Checks the phone's HMAC in constant time.
fn verify(token: &str, request: &PairRequest) -> bool {
    let Ok(given) = STANDARD.decode(&request.mac) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    mac.update(mac_message(&request.public_key, &request.device).as_bytes());
    mac.verify_slice(&given).is_ok()
}

#[derive(Debug, Serialize)]
struct Reply<'a> {
    status: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    phrase: Option<&'a str>,
}

fn respond(request: Request, code: u16, status: &str, message: &str, phrase: Option<&str>) {
    let body = serde_json::to_string(&Reply {
        status,
        message,
        phrase,
    })
    .unwrap();
    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
    // The phone has gone or given up; there is no one left to tell.
    let _ = request.respond(
        Response::from_string(body)
            .with_status_code(code)
            .with_header(header),
    );
}

/// The phone that was paired.
#[derive(Debug)]
pub struct Paired {
    pub key: PublicKey,
    pub device: String,
    /// The push secret the phone sent, opened. `None` when it sent none.
    pub push: Option<anyhow::Result<PushGrant>>,
}

/// What the listener asks of the rest of the tool. A trait, so the tests
/// can answer the question without a terminal.
pub trait Host {
    /// Shows the phone and its key, and asks whether to let it in.
    fn approve(&mut self, device: &str, phrase: &str) -> anyhow::Result<bool>;
    /// Writes the key. Called only after `approve` said yes.
    fn install(&mut self, key: &PublicKey, device: &str) -> anyhow::Result<()>;
}

/// Opens the listener. On the advertised address when that is one of this
/// machine's own — a Tailscale address, usually — so the port is not open
/// on every interface; on all of them otherwise, because the advertised
/// address may be a public IP that belongs to a NAT in front of the box.
pub fn listen(host: &str, port: u16) -> anyhow::Result<(Server, String)> {
    if let Ok(server) = Server::http((host, port)) {
        return Ok((server, format!("{host}:{port}")));
    }
    let server = Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow::anyhow!("could not listen on port {port}: {e}"))?;
    Ok((server, format!("0.0.0.0:{port}")))
}

/// Waits for one phone, until `timeout` runs out.
///
/// One pairing per code: after a phone is let in, or turned away, the
/// listener closes and the token is spent. A person who says no at the
/// terminal did not expect that phone, which means the code was seen by
/// someone else, and a code someone else has seen must not stay open.
pub fn serve(
    server: &Server,
    token: &str,
    timeout: Duration,
    host: &mut dyn Host,
) -> anyhow::Result<Paired> {
    let deadline = Instant::now() + timeout;
    let mut bad_attempts = 0;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "no phone paired within {} minutes. Run this again for a new code",
                timeout.as_secs() / 60
            );
        }
        let Some(mut request) = server
            .recv_timeout(remaining)
            .context("the listener failed")?
        else {
            continue;
        };

        if request.url() != PATH {
            respond(request, 404, "not_found", "Not a pairing address.", None);
            continue;
        }
        if *request.method() != Method::Post {
            respond(request, 405, "bad_request", "Use POST.", None);
            continue;
        }

        let mut body = String::new();
        if request
            .as_reader()
            .take(MAX_BODY)
            .read_to_string(&mut body)
            .is_err()
        {
            respond(request, 400, "bad_request", "The body is not UTF-8.", None);
            continue;
        }
        let pair: PairRequest = match serde_json::from_str(&body) {
            Ok(pair) => pair,
            Err(_) => {
                respond(
                    request,
                    400,
                    "bad_request",
                    "The body is not a pairing request.",
                    None,
                );
                continue;
            }
        };

        if !verify(token, &pair) {
            bad_attempts += 1;
            respond(
                request,
                401,
                "bad_token",
                "This request was not made from the current pairing code.",
                None,
            );
            if bad_attempts >= MAX_BAD_ATTEMPTS {
                bail!(
                    "{bad_attempts} requests came with a wrong token. Stopped listening. Run this again for a new code"
                );
            }
            continue;
        }

        let key = match PublicKey::parse(&pair.public_key) {
            Ok(key) => key,
            Err(e) => {
                respond(request, 400, "bad_key", &format!("{e:#}"), None);
                continue;
            }
        };
        let device = device_slug(&pair.device);
        let phrase = key.phrase();

        if !host.approve(&device, &phrase)? {
            respond(
                request,
                403,
                "declined",
                "The key was declined on the server.",
                Some(&phrase),
            );
            bail!("declined. The pairing code is spent; run this again for a new one");
        }
        if let Err(e) = host.install(&key, &device) {
            respond(
                request,
                500,
                "failed",
                "The server could not save the key.",
                Some(&phrase),
            );
            return Err(e);
        }
        respond(request, 200, "accepted", "Paired.", Some(&phrase));
        // Opened after the key is in, not before: a seal that does not
        // open costs the notifications, never the pairing.
        let push = pair.push.as_ref().map(|p| p.open(token));
        return Ok(Paired { key, device, push });
    }
}

/// The options in front of a paired key, which say what it may do.
///
/// The app needs exactly two things over SSH: a PTY, to type
/// `herdr session attach`, and a direct-tcpip channel to the bridge port.
/// Grant those and nothing else, so a stolen phone key cannot forward
/// anywhere. `permitopen` may be repeated, and each one is another address
/// the key may open a forwarded connection to — the Ports tab's extra
/// forwards.
pub fn key_options(target_port: u16, forwards: &Forwards, no_restrict: bool) -> String {
    // `restrict` needs sshd 7.2 or newer. An older one does not know the
    // option and quietly ignores the whole key, so --no-restrict drops the
    // prefix and trades the limits for a login that works.
    if no_restrict {
        return String::new();
    }
    if forwards.any {
        return "restrict,pty,port-forwarding".into();
    }
    let mut options =
        format!("restrict,pty,port-forwarding,permitopen=\"127.0.0.1:{target_port}\"");
    for (host, port) in &forwards.to {
        options.push_str(&format!(",permitopen=\"{host}:{port}\""));
    }
    options
}

/// What `--forward-port` asked for.
#[derive(Debug, Default)]
pub struct Forwards {
    pub any: bool,
    pub to: Vec<(String, u16)>,
}

impl Forwards {
    /// Each spec is a port, a `host:port` the machine can reach, or `any`.
    /// A bare port means 127.0.0.1, because that is where the app forwards
    /// in nearly every case.
    pub fn parse(specs: &[String]) -> anyhow::Result<Self> {
        let mut forwards = Self::default();
        for spec in specs {
            if spec.eq_ignore_ascii_case("any") {
                forwards.any = true;
                continue;
            }
            let (host, port) = spec.rsplit_once(':').unwrap_or(("127.0.0.1", spec));
            let valid_host = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-[]:".contains(c));
            let port: u16 = port.parse().ok().filter(|_| valid_host).with_context(|| {
                format!("--forward-port wants a port, a host:port, or 'any': {spec}")
            })?;
            forwards.to.push((host.to_string(), port));
        }
        Ok(forwards)
    }

    /// One phrase for the lines that report what a key may do.
    pub fn describe(&self, target_port: u16) -> String {
        if self.any {
            return "a PTY, forwarding anywhere".into();
        }
        let mut to = vec![format!("127.0.0.1:{target_port}")];
        to.extend(self.to.iter().map(|(h, p)| format!("{h}:{p}")));
        format!("a PTY and {}", to.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::tests::sample_line;
    use std::io::Write;
    use std::net::TcpStream;

    #[test]
    fn options_allow_a_pty_and_the_listed_forwards() {
        let forwards = Forwards::parse(&["3000".into(), "db.local:5432".into()]).unwrap();
        assert_eq!(
            key_options(4499, &forwards, false),
            r#"restrict,pty,port-forwarding,permitopen="127.0.0.1:4499",permitopen="127.0.0.1:3000",permitopen="db.local:5432""#
        );
        assert_eq!(
            key_options(4499, &Forwards::parse(&["any".into()]).unwrap(), false),
            "restrict,pty,port-forwarding"
        );
        assert_eq!(key_options(4499, &Forwards::default(), true), "");
        assert!(Forwards::parse(&["x".into()]).is_err());
        // A quote would end the option early and let the rest through.
        assert!(Forwards::parse(&["a\"b:1".into()]).is_err());
    }

    struct Answer {
        yes: bool,
        installed: Vec<String>,
    }

    impl Host for Answer {
        fn approve(&mut self, _: &str, _: &str) -> anyhow::Result<bool> {
            Ok(self.yes)
        }
        fn install(&mut self, _: &PublicKey, device: &str) -> anyhow::Result<()> {
            self.installed.push(device.into());
            Ok(())
        }
    }

    /// Sends one raw HTTP request and returns the status line.
    fn post(addr: std::net::SocketAddr, body: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(
            stream,
            "POST {PATH} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply.lines().next().unwrap_or_default().to_string()
    }

    fn run(yes: bool, requests: Vec<String>) -> (anyhow::Result<Paired>, Vec<String>, Vec<String>) {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let client =
            std::thread::spawn(move || requests.iter().map(|b| post(addr, b)).collect::<Vec<_>>());
        let mut host = Answer {
            yes,
            installed: vec![],
        };
        let result = serve(&server, "tok", Duration::from_secs(5), &mut host);
        (result, client.join().unwrap(), host.installed)
    }

    fn request(token: &str, device: &str) -> String {
        let public_key = sample_line(9);
        serde_json::to_string(&PairRequest {
            mac: sign(token, &public_key, device),
            public_key,
            device: device.into(),
            push: None,
        })
        .unwrap()
    }

    const SECRET: &str = "4bdhpjaLlBQ68G5j8_SGmJHUL6Ysj28qYoLDmU5BsME";

    #[test]
    fn a_pairing_carries_the_push_secret() {
        let public_key = sample_line(9);
        let body = serde_json::to_string(&PairRequest {
            mac: sign("tok", &public_key, "x"),
            public_key,
            device: "x".into(),
            push: Some(PushRequest::seal("tok", "C0FFEE-1", SECRET)),
        })
        .unwrap();
        let (result, _, _) = run(true, vec![body]);
        let grant = result.unwrap().push.unwrap().unwrap();
        assert_eq!(
            grant,
            PushGrant {
                connection_id: "C0FFEE-1".into(),
                secret: SECRET.into()
            }
        );
    }

    #[test]
    fn a_push_secret_opens_only_with_its_token_and_connection() {
        let sealed = PushRequest::seal("tok", "C1", SECRET);
        assert!(sealed.open("tok").is_ok());
        assert!(sealed.open("other").is_err());
        let moved = PushRequest {
            connection_id: "C2".into(),
            sealed: sealed.sealed.clone(),
        };
        assert!(moved.open("tok").is_err());
        let bad_id = PushRequest {
            connection_id: "a b".into(),
            sealed: sealed.sealed,
        };
        assert!(bad_id.open("tok").is_err());
    }

    /// The app's `PairingExchangeTests` seals the same secret with the same
    /// nonce and expects these same bytes. A change to the key, the label
    /// or the layout on one side breaks a test, not a phone.
    #[test]
    fn the_seal_matches_the_app() {
        assert_eq!(
            PushRequest::seal("tok", "C1", SECRET).sealed,
            "BwcHBwcHBwcHBwcHrr6EtxsB0pwu1YAqRGJK0JjfHo7hLhBB21vICn9MKqVwkwHIUiPPesGMtvInNIQKXK+eNeRb+Sgftnw="
        );
    }

    #[test]
    fn a_signed_key_that_is_approved_is_installed() {
        let (result, statuses, installed) = run(true, vec![request("tok", "My iPhone")]);
        assert!(statuses[0].contains("200"));
        assert_eq!(result.unwrap().device, "My-iPhone");
        assert_eq!(installed, ["My-iPhone"]);
    }

    #[test]
    fn a_wrong_token_is_refused_and_the_listener_keeps_waiting() {
        let (result, statuses, installed) =
            run(true, vec![request("guess", "x"), request("tok", "x")]);
        assert!(statuses[0].contains("401"));
        assert!(statuses[1].contains("200"));
        assert!(result.is_ok());
        assert_eq!(installed.len(), 1);
    }

    #[test]
    fn a_declined_key_spends_the_code() {
        let (result, statuses, installed) = run(false, vec![request("tok", "x")]);
        assert!(statuses[0].contains("403"));
        assert!(result.is_err());
        assert!(installed.is_empty());
    }

    #[test]
    fn the_device_name_is_covered_by_the_mac() {
        let mut tampered: PairRequest = serde_json::from_str(&request("tok", "mine")).unwrap();
        tampered.device = "someone-else".into();
        let (_, statuses, _) = run(
            true,
            vec![
                serde_json::to_string(&tampered).unwrap(),
                request("tok", "x"),
            ],
        );
        assert!(statuses[0].contains("401"));
    }
}

