# legio-cli

The `legio` command. It prepares a Linux machine so the HerdrPOC iOS
app can connect to it, and pairs phones with it. It replaces
`HerdrPOC/Scripts/vps-setup.sh`.

The main change from the script: **the phone makes its own SSH key.** The
script made the key on the server and showed the private half in a QR
code. Now the QR code holds no key. The phone sends only its public key
back, and you confirm the fingerprint on the terminal before the key is
added. See [PROTOCOL.md](PROTOCOL.md).

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

`setup` does these steps:

1. Finds a proxy: `systemd-socket-proxyd` first, then socat. It installs
   socat only when systemd has no proxy.
2. Checks that the Herdr socket exists.
3. Installs a systemd user unit that keeps 127.0.0.1:4499 open to the
   socket. Then it asks Herdr if it knows `agent.kinds`.
4. Checks the SSH server: key login, the `authorized_keys` path, password
   login, and the permissions on your home directory.
5. Shows the pairing QR code and waits for the phone.

| Command | What it does |
| --- | --- |
| `setup` | All of the steps above. `--no-pair` stops after step 4. |
| `pair` | Step 5 only. Works on any machine with sshd, also a Mac. |
| `devices` | Lists the paired phones and what each one may do. |
| `unpair <name or fingerprint>` | Removes one phone. `--all` removes all phones. `--legacy` removes the key that `vps-setup.sh` made. |
| `options` | Applies `--forward-port` or `--no-restrict` to the phones that are already paired. |
| `check` | Reports on the bridge, Herdr, sshd and the paired phones. Changes nothing. |
| `uninstall` | Removes the units, every phone key, and the legacy key. |

Frequent options:

- `--host <addr>` — the address the phone dials. The default is the
  Tailscale address, then the public IP.
- `--forward-port <port | host:port | any>` — another address the app's
  Ports tab may forward to. Repeat it for more than one.
- `--pair-port <port>` — the port the phone sends its key to. The default
  is 7450. The phone must reach it, so open it in the firewall for the
  pairing, or pair over Tailscale.
- `--no-qr` — print the pairing code as JSON, for the app's Paste button.

## What it changes

- `~/.config/systemd/user/herdr-bridge.{socket,service}`
- `~/.ssh/authorized_keys` — one line for each phone, with the comment
  `herdr-app:<phone>`. The tool copies the file to
  `authorized_keys.herdr-backup-<time>` before each change, and replaces
  the file in one rename. It keeps lines that are not its own byte for
  byte. It never edits a line that holds two keys (damage from an old
  version of the script). It reports such a line.
- Lingering for your user (`loginctl enable-linger`), through `sudo`.

It never edits the system sshd configuration.

## Moving from vps-setup.sh

The key that the script made is at `~/.ssh/herdr-poc`. Its private half
was shown as a QR code. After a phone pairs with `legio`, the tool
asks if it can remove that key. A phone that still uses the old key stops
working when you remove it.

The app must read pairing code version 2 before it can pair with this
tool. The app of today reads version 1 only.

## Test

```sh
cargo test
```

The tests play the phone against the real listener, and edit
`authorized_keys` text in the ways that broke the old script.
