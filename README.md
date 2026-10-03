# legio-cli

The `legio` command. It prepares a Linux machine or a Mac so the Legio
iOS app can connect to it, and pairs phones with it.

**The phone makes its own SSH key.** The QR code holds no key. The phone
sends only its public key back, and you confirm a five-word key phrase on
the terminal before the key is added. See [PROTOCOL.md](PROTOCOL.md).

## Install

Each `v*` tag builds a release for macOS (arm64, x86_64) and Linux
(x86_64, aarch64). [`install.sh`](install.sh) finds the build for your
machine, checks it against the release's `SHA256SUMS`, and puts `legio`
in `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/jrobertojunior/legio-cli/master/install.sh | sh
```

- `LEGIO_VERSION=v0.1.0` installs that tag. The default is the latest release.
- `LEGIO_DIR=/usr/local/bin` installs there. The default is `~/.local/bin`.

For example: `curl -fsSL .../install.sh | LEGIO_VERSION=v0.1.0 sh`.

## Update

```sh
legio update
```

`update` downloads the latest release for this machine and checks it
against `SHA256SUMS`. Then it runs the new binary one time, and renames
it over the old one. If a step fails, the old binary stays.

- `legio update --check` only says whether a newer release is out.
- `legio update --version v0.1.0` installs that release, also an older
  one. Use it to go back from a bad release.
- `legio update --force` installs the latest release again.

After the update, the new binary runs `legio after-update`. A release
uses it for its own work, for example to restart a service that it
changed.

v0.1.0 has no `update` command. To go from v0.1.0 to a newer version,
run `install.sh` again.

## Release

1. Set `version` in `Cargo.toml`, run `cargo build`, and commit
   `Cargo.toml` and `Cargo.lock`.
2. Push a tag with the same version:

   ```sh
   git tag v0.1.1 && git push origin v0.1.1
   ```

The release workflow builds the four binaries, writes `SHA256SUMS`, and
publishes the release. Machines get it with `legio update`.

## Build

```sh
cargo build --release
```

Build it on the server, or build for Linux from another machine, for
example with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```sh
cargo zigbuild --release --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/legio server:
```

## Use

Run it on the server, as the user the app logs in as:

```sh
./legio            # same as: legio setup
```

`setup` does these steps on Linux:

1. Finds a proxy: `systemd-socket-proxyd` first, then socat. It installs
   socat only when systemd has no proxy.
2. Checks that the Herdr socket exists.
3. Installs a systemd user unit that keeps 127.0.0.1:4499 open to the
   socket. Then it asks Herdr if it knows `agent.kinds`.
4. Installs the notification watcher as a systemd user unit (see
   [Notifications](#notifications)).
5. Checks the SSH server: key login, the `authorized_keys` path, password
   login, and the permissions on your home directory.
6. Shows the pairing QR code and waits for the phone.

On a Mac, steps 1 to 3 are different. `legio` has
[`scripts/mac-setup.sh`](scripts/mac-setup.sh) built into its binary, and
runs it in place of the systemd units. The script checks Remote Login,
installs socat with Homebrew, and writes a `com.legio.bridge` LaunchAgent
that keeps 127.0.0.1:4499 open to the socket. `--on-demand` lets launchd
hold the port and start one socat for each connection. Steps 4 to 6 are
the same as on Linux, but step 4 writes a `com.legio.watch` LaunchAgent.

| Command | What it does |
| --- | --- |
| `setup` | All of the steps above. `--no-pair` stops after step 5. |
| `pair` | Step 6 only. Works on any machine with sshd. |
| `devices` | Lists the paired phones, each with its key phrase, and what each one may do. |
| `unpair <name or key phrase>` | Removes one phone. `--all` removes all phones. |
| `options` | Applies `--forward-port` or `--no-restrict` to the phones that are already paired. |
| `check` | Reports on the bridge, Herdr, the notification watcher, sshd and the paired phones. Changes nothing. |
| `push add <secret> --connection <id>` | Adds a phone to the notification list. The app runs it. Reads the secret from stdin when it is left out. |
| `push list` | Lists the phones that get notifications. |
| `push remove <name or secret start>` | Stops sending to one phone. `--all` stops sending to all phones. |
| `push test` | Sends a test notification to every phone. |
| `watch` | Runs the notification watcher. The service runs it; you do not. |
| `update` | Replaces `legio` with the latest release. See [Update](#update). |
| `uninstall` | Removes the units (the LaunchAgents on a Mac), the notification list, and every phone key. |

Frequent options:

- `--host <addr>` — the address the phone dials. Give an IP address: the
  app cannot resolve `.local` names. The default is the Tailscale
  address, then (on a Mac) the Wi-Fi address, then the public IP.
- `--forward-port <port | host:port | any>` — another address the app's
  Ports tab may forward to. Repeat it for more than one.
- `--pair-port <port>` — the port the phone sends its key to. The default
  is 7450. The phone must reach it, so open it in the firewall for the
  pairing, or pair over Tailscale.
- `--no-qr` — print the pairing code as JSON, for the app's Paste button.

## Notifications

iOS stops the app's SSH connection soon after the app goes to the
background. So this machine tells the phone when an agent needs it.

`legio watch` reads Herdr's `session.snapshot` every 2 seconds. When an
agent changes to `blocked` or `done` and keeps that status for two
snapshots, it sends a push to each phone in
`~/.config/legio/push.json`:

| Status | Notification | Level |
| --- | --- | --- |
| `blocked` | "Claude needs input" | time-sensitive |
| `done` | "Claude finished" | active |

The push goes through the relay at `https://legiorelay.jrobe.cloud`
([`legio-relay`](https://github.com/jrobertojunior/legio-relay)). The relay
holds the APNs key. This machine holds only one device secret for each
phone, which the app gives it with `legio push add`. Set `LEGIO_RELAY` to
use another relay.

The service runs this same `legio` binary. If you move or reinstall
`legio`, run `legio setup` again.

## What it changes

- `~/.config/systemd/user/herdr-bridge.{socket,service}` on Linux.
- `~/Library/LaunchAgents/com.legio.bridge.plist` on a Mac, with its log
  in `~/Library/Logs/com.legio.bridge.log`.
- `~/.config/systemd/user/legio-watch.service` on Linux, or
  `~/Library/LaunchAgents/com.legio.watch.plist` on a Mac, with its log in
  `~/Library/Logs/com.legio.watch.log`.
- `~/.config/legio/push.json` (mode 600) — the device secrets of the
  phones that get notifications.
- `~/.ssh/authorized_keys` — one line for each phone, with the comment
  `legio-app:<phone>`. The tool copies the file to
  `authorized_keys.herdr-backup-<time>` before each change, and replaces
  the file in one rename. It keeps lines that are not its own byte for
  byte. It never edits a line that holds two keys (what an append without
  a newline guard makes). It reports such a line.
- Lingering for your user (`loginctl enable-linger`), through `sudo`, on
  Linux.

It never edits the system sshd configuration.

## Test

```sh
cargo test
```

The tests play the phone against the real listener, and edit
`authorized_keys` text in the ways that break a careless edit.
