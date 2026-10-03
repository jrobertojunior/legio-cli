//! `legio` — prepare a machine so the Legio iOS app can connect,
//! and pair phones with it.
//!
//! It installs the bridge to Herdr's socket as systemd user units on Linux,
//! or as a LaunchAgent on a Mac, checks the SSH server, and pairs phones. A phone makes its own key and sends
//! only the public half. See `pairing.rs`.

mod authorized_keys;
mod bridge;
mod mac;
mod pairing;
mod phrase;
mod push;
mod server;
mod sshkey;
mod sys;
mod ui;
mod update;
mod watch;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;

use crate::authorized_keys as keys;
use crate::pairing::{Forwards, Host, Payload};
use crate::sshkey::PublicKey;
use crate::ui::{bold, info, warn};

/// Prepare this machine for the Legio app, and pair phones with it.
///
/// With no command, runs `setup`: the bridge, the checks, and one pairing.
#[derive(Parser)]
#[command(name = "legio", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    setup: SetupArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Install the bridge, check the SSH server, and pair a phone.
    Setup(SetupArgs),
    /// Pair a phone only. Works on any machine with sshd, systemd or not.
    Pair {
        #[command(flatten)]
        bridge: TargetArgs,
        #[command(flatten)]
        pair: PairArgs,
    },
    /// List the phones paired with this machine.
    Devices,
    /// Remove a paired phone's key.
    Unpair {
        /// The phone's name or its key phrase, as `devices` prints them.
        #[arg(required_unless_present = "all")]
        device: Option<String>,
        /// Remove every paired phone.
        #[arg(long, conflicts_with = "device")]
        all: bool,
    },
    /// Change what paired phones may do, without pairing a new one.
    Options {
        #[command(flatten)]
        bridge: TargetArgs,
        #[command(flatten)]
        restrict: RestrictArgs,
    },
    /// Report on the bridge, Herdr, and the SSH server. Changes nothing.
    Check {
        #[command(flatten)]
        bridge: TargetArgs,
        #[command(flatten)]
        herdr: HerdrArgs,
    },
    /// Remove the bridge units and every paired phone's key.
    Uninstall,
    /// Replace this binary with the latest release.
    Update {
        /// Install this release, for example v0.2.0. It may be older than
        /// this one: that is how you go back from a bad release.
        #[arg(long)]
        version: Option<String>,
        /// Only say whether a newer release is out.
        #[arg(long, conflicts_with = "version")]
        check: bool,
        /// Install the latest release even when this is the same version.
        #[arg(long)]
        force: bool,
    },
    /// Run by `update` with the new binary, so it can finish its own update.
    #[command(hide = true)]
    AfterUpdate {
        /// The version that ran the update.
        #[arg(long)]
        from: String,
    },
    /// Send a push to the phones when an agent needs input or finishes.
    /// `setup` installs this as a service; you do not run it by hand.
    Watch {
        #[command(flatten)]
        herdr: HerdrArgs,
    },
    /// The phones that get push notifications from this machine.
    #[command(subcommand)]
    Push(PushCommand),
}

#[derive(Subcommand)]
enum PushCommand {
    /// Add a phone. The app runs this when it connects.
    Add {
        /// The device secret the relay gave the phone. Read from stdin
        /// when left out, so it does not show in the process list.
        secret: Option<String>,
        /// The app's id for its connection to this machine.
        #[arg(long)]
        connection: String,
        /// The phone's name.
        #[arg(long, default_value = "")]
        name: String,
    },
    /// List the phones that get notifications.
    List,
    /// Stop sending to a phone.
    Remove {
        /// The phone's name, or the start of its secret, as `list` prints them.
        #[arg(required_unless_present = "all")]
        device: Option<String>,
        /// Stop sending to every phone.
        #[arg(long, conflicts_with = "device")]
        all: bool,
    },
    /// Send a test notification to every phone.
    Test,
}

#[derive(Args, Clone)]
struct TargetArgs {
    /// The TCP port the bridge listens on, on 127.0.0.1.
    #[arg(long, default_value_t = 4499)]
    port: u16,
}

#[derive(Args, Clone)]
struct HerdrArgs {
    /// Herdr's socket. Defaults to ~/.config/herdr/herdr.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// The Herdr session the app attaches panes from.
    #[arg(long, default_value = "default")]
    session: String,
}

#[derive(Args, Clone)]
struct RestrictArgs {
    /// Another address phones may forward to from the Ports tab: a port,
    /// a host:port this machine can reach, or `any`. Repeat for more.
    #[arg(long = "forward-port", value_name = "PORT")]
    forward_ports: Vec<String>,
    /// Write keys with no restrictions. For sshd older than 7.2, which does
    /// not know `restrict` and ignores the whole key.
    #[arg(long)]
    no_restrict: bool,
}

#[derive(Args, Clone)]
struct PairArgs {
    #[command(flatten)]
    herdr: HerdrArgs,
    #[command(flatten)]
    restrict: RestrictArgs,
    /// The address the phone dials. Defaults to the Tailscale address, then
    /// the public IP.
    #[arg(long)]
    host: Option<String>,
    /// The port the phone sends its public key to while pairing.
    #[arg(long, default_value_t = 7450)]
    pair_port: u16,
    /// How long to wait for the phone, in minutes.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
    /// Print the pairing code as JSON instead of a QR code.
    #[arg(long)]
    no_qr: bool,
}

#[derive(Args, Clone)]
struct SetupArgs {
    #[command(flatten)]
    target: TargetArgs,
    #[command(flatten)]
    pair: PairArgs,
    /// Use socat even when systemd has its own proxy.
    #[arg(long)]
    use_socat: bool,
    /// On a Mac, let launchd hold the port and start one socat for each
    /// connection, instead of one socat that always runs.
    #[arg(long)]
    on_demand: bool,
    /// Install the bridge and run the checks, but pair no phone.
    #[arg(long)]
    no_pair: bool,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Setup(cli.setup)) {
        Command::Setup(args) => setup(args),
        Command::Pair { bridge, pair } => pair_phone(&bridge, &pair),
        Command::Devices => devices(),
        Command::Unpair { device, all } => unpair(device.as_deref(), all),
        Command::Options { bridge, restrict } => set_options(&bridge, &restrict).map(|_| ()),
        Command::Check { bridge, herdr } => check(&bridge, &herdr),
        Command::Uninstall => uninstall(),
        Command::Update {
            version,
            check,
            force,
        } => update::run(version.as_deref(), check, force),
        Command::AfterUpdate { from } => update::after_update(&from),
        Command::Watch { herdr } => socket_path(&herdr).and_then(|s| watch::run(&s, &sys::home()?)),
        Command::Push(command) => push_command(command),
    };
    if let Err(e) = result {
        eprintln!("\x1b[31mERROR: {e:#}\x1b[0m");
        std::process::exit(1);
    }
}

fn socket_path(herdr: &HerdrArgs) -> anyhow::Result<PathBuf> {
    match &herdr.socket {
        Some(path) => Ok(path.clone()),
        None => Ok(sys::home()?.join(".config/herdr/herdr.sock")),
    }
}

fn setup(args: SetupArgs) -> anyhow::Result<()> {
    let home = sys::home()?;
    let user = sys::user()?;
    let port = args.target.port;
    let herdr = &args.pair.herdr;
    let socket = socket_path(herdr)?;

    if cfg!(target_os = "macos") {
        mac::install(port, &socket, &herdr.session, args.on_demand)?;
    } else {
        bold("1. Choosing the proxy");
        bridge::require_systemd()?;
        let proxy = bridge::choose_proxy(args.use_socat)?;
        bridge::report_herdr(&herdr.session);

        bold("2. Checking the Herdr socket");
        bridge::report_socket(&socket);

        bold("3. Installing the systemd user units");
        bridge::ensure_linger(&user);
        bridge::install_units(&home, &proxy, port, &socket)?;
        bridge::probe_agent_kinds(port);
    }

    // The bridge carries the app while it is open. The watcher is what
    // reaches the phone when it is not.
    bold("4. Installing the notification watcher");
    if let Err(e) = watch::install(&home, &socket) {
        warn(&format!("{e:#}"));
        warn("The app works without it, but sends no notifications.");
    }

    // Before the pairing, not after: a server that will refuse the key is
    // worth knowing about before the phone is in your hand.
    bold("5. Checking the SSH server");
    server::fix_home_permissions(&home);
    server::check_sshd();

    if args.no_pair {
        set_options(&args.target, &args.pair.restrict)?;
    } else {
        bold("6. Pairing the phone");
        pair_phone(&args.target, &args.pair)?;
    }

    println!();
    bold("Done.");
    if cfg!(target_os = "macos") {
        info(&format!(
            "Read the proxy log with: cat {}",
            mac::log_file(&home).display()
        ));
    } else {
        info(&format!(
            "Read the proxy log with: journalctl --user -u {} -f",
            bridge::UNIT_NAME
        ));
    }
    info("Pair another phone with: legio pair");
    info("Remove everything with: legio uninstall");
    Ok(())
}

/// Writes the key the listener was handed, after the person said yes.
struct Installer {
    file: keys::File,
    options: String,
    describe: String,
}

impl Host for Installer {
    fn approve(&mut self, device: &str, phrase: &str) -> anyhow::Result<bool> {
        println!();
        bold(&format!("A phone sent its key: {device}"));
        info("The app shows five words. Check that they match these:");
        ui::phrase(phrase);
        if keys::devices(&self.file.read()?)
            .iter()
            .any(|d| d.name == device)
        {
            info(&format!(
                "This replaces the key already paired as {device}."
            ));
        }
        ui::confirm("Add it to authorized_keys?")
    }

    fn install(&mut self, key: &PublicKey, device: &str) -> anyhow::Result<()> {
        let (text, replaced) = keys::with_device(&self.file.read()?, key, device, &self.options);
        if let Some(backup) = self.file.write(&text)? {
            info(&format!(
                "Backed up authorized_keys to {}.",
                backup.display()
            ));
        }
        for old in replaced {
            info(&format!(
                "Removed the old key of {} ({}).",
                old.name,
                old.phrase()
            ));
        }
        if self.options.is_empty() {
            warn(&format!(
                "Added the key to {} with no restrictions.",
                self.file.path.display()
            ));
        } else {
            info(&format!(
                "Added the key to {}, limited to {}.",
                self.file.path.display(),
                self.describe
            ));
        }
        Ok(())
    }
}

fn pair_phone(target: &TargetArgs, args: &PairArgs) -> anyhow::Result<()> {
    let home = sys::home()?;
    // The options first, so a --forward-port given with a new pairing also
    // reaches the phones that are already paired.
    let (options, describe) = set_options(target, &args.restrict)?;

    let address = server::Address::find(args.host.as_deref());
    address.report();
    let host_key = server::host_key_fingerprint();
    if host_key.is_none() {
        warn("Could not read /etc/ssh/ssh_host_ed25519_key.pub. The app will not be");
        warn("able to check that it is talking to this machine.");
    }

    let token = pairing::new_token()?;
    let (listener, bound) = pairing::listen(address.host(), args.pair_port)?;
    let payload = Payload {
        v: pairing::PAYLOAD_VERSION,
        name: sys::hostname(),
        host: address.host().to_string(),
        port: server::ssh_port(),
        user: sys::user()?,
        target_port: target.port,
        session: args.herdr.session.clone(),
        host_key,
        pair_port: args.pair_port,
        token: token.clone(),
    };
    let json = serde_json::to_string(&payload)?;

    println!();
    if args.no_qr {
        bold("In the app tap + -> \"Scan pairing code\" -> Paste, then paste this:");
        println!();
        println!("{json}");
    } else {
        bold("In the app tap + -> \"Scan pairing code\", then scan this:");
        println!();
        let code =
            QrCode::new(json.as_bytes()).context("the pairing code does not fit in a QR code")?;
        // Black on white whatever the terminal's theme, as `qrencode -t
        // ANSIUTF8` drew it: the blocks draw the *light* modules in white on
        // a black background. Left to the theme, a dark terminal shows the
        // code inverted, and not every phone camera reads that.
        let rendered = code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build();
        for line in rendered.lines() {
            println!("\x1b[97;40m{line}\x1b[0m");
        }
        info("The code holds no key. It is good for one phone, for the next");
        info(&format!(
            "{} minutes, and you confirm that phone here before it is let in.",
            args.timeout
        ));
    }
    println!();
    info(&format!("Waiting for the phone on {bound} …"));
    info(&format!(
        "The phone must reach port {} on {}. Open it in the firewall if needed.",
        args.pair_port,
        address.host()
    ));

    let mut installer = Installer {
        file: keys::File::in_home(&home),
        options,
        describe,
    };
    let paired = pairing::serve(
        &listener,
        &token,
        Duration::from_secs(args.timeout * 60),
        &mut installer,
    )?;
    drop(listener);

    println!();
    bold(&format!(
        "Paired {} ({}).",
        paired.device,
        paired.key.phrase()
    ));
    Ok(())
}

/// Rewrites what every paired phone may do, when it differs from what was
/// asked for now. Returns the options and a phrase describing them.
///
/// This exists because of --forward-port: without it a run that asks for
/// another forwarded port would change nothing on a machine that is
/// already paired, which is all of them.
fn set_options(target: &TargetArgs, args: &RestrictArgs) -> anyhow::Result<(String, String)> {
    let forwards = Forwards::parse(&args.forward_ports)?;
    let options = pairing::key_options(target.port, &forwards, args.no_restrict);
    let describe = forwards.describe(target.port);

    let file = keys::File::in_home(&sys::home()?);
    let (text, changed) = keys::with_options(&file.read()?, &options);
    if changed > 0 {
        file.write(&text)?;
        info(&format!(
            "Updated {changed} paired phone(s): now limited to {describe}."
        ));
    }
    Ok((options, describe))
}

fn devices() -> anyhow::Result<()> {
    let text = keys::File::in_home(&sys::home()?).read()?;
    let found = keys::devices(&text);
    if found.is_empty() {
        info("No phones are paired with this machine.");
    }
    for device in &found {
        println!("{:<24} {}", device.name, device.phrase());
        let options = if device.options.is_empty() {
            "(no restrictions)"
        } else {
            &device.options
        };
        info(options);
    }
    warn_joined(&text);
    Ok(())
}

fn warn_joined(text: &str) {
    let joined = keys::joined_lines(text);
    if joined > 0 {
        warn(&format!(
            "{joined} line(s) in authorized_keys carry more than one key."
        ));
        warn("An append without a newline guard joins lines like this. They were left alone.");
        warn("Split them by hand: each key belongs on its own line.");
    }
}

fn unpair(device: Option<&str>, all: bool) -> anyhow::Result<()> {
    let file = keys::File::in_home(&sys::home()?);
    let text = file.read()?;
    let (text, removed) = keys::without(&text, |blob, comment| {
        let Some(name) = comment.strip_prefix(keys::MARKER) else {
            return false;
        };
        all || device
            .is_some_and(|d| d == name || sshkey::phrase_of_blob(blob).is_ok_and(|p| p == d))
    });
    if removed == 0 {
        anyhow::bail!("no paired phone matches. List them with: legio devices");
    }
    file.write(&text)?;
    info(&format!(
        "Removed {removed} key(s) from {}.",
        file.path.display()
    ));
    warn_joined(&text);
    Ok(())
}

fn check(target: &TargetArgs, herdr: &HerdrArgs) -> anyhow::Result<()> {
    let home = sys::home()?;
    bold("Bridge");
    if cfg!(target_os = "macos") {
        mac::report_agent(&home, target.port);
    } else {
        match bridge::installed_unit(&home) {
            Some(unit) => bridge::report_unit(&unit, target.port),
            None => warn("No bridge unit is installed. Run: legio setup"),
        }
    }
    bold("Herdr");
    bridge::report_herdr(&herdr.session);
    bridge::report_socket(&socket_path(herdr)?);
    bridge::probe_agent_kinds(target.port);
    bold("SSH server");
    server::check_sshd();
    bold("Notifications");
    watch::report(&home);
    bold("Paired phones");
    devices()
}

fn push_command(command: PushCommand) -> anyhow::Result<()> {
    let path = push::file(&sys::home()?);
    match command {
        PushCommand::Add {
            secret,
            connection,
            name,
        } => {
            let secret = match secret {
                Some(s) => s,
                None => {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim().to_string()
                }
            };
            push::check_secret(&secret)?;
            let mut devices = push::load(&path)?;
            let added = push::upsert(
                &mut devices,
                push::Device {
                    secret,
                    connection_id: connection,
                    name,
                },
            );
            push::save(&path, &devices)?;
            info(if added {
                "Added the phone."
            } else {
                "Updated the phone."
            });
        }
        PushCommand::List => {
            let devices = push::load(&path)?;
            if devices.is_empty() {
                info("No phone gets notifications from this machine.");
            }
            for device in &devices {
                let name = if device.name.is_empty() {
                    "(no name)"
                } else {
                    &device.name
                };
                println!("{name:<24} {}…", device.short_secret());
            }
        }
        PushCommand::Remove { device, all } => {
            let mut devices = push::load(&path)?;
            let before = devices.len();
            devices.retain(|d| {
                !(all
                    || device
                        .as_deref()
                        .is_some_and(|x| x == d.name || d.secret.starts_with(x)))
            });
            let removed = before - devices.len();
            if removed == 0 {
                anyhow::bail!("no phone matches. List them with: legio push list");
            }
            push::save(&path, &devices)?;
            info(&format!("Removed {removed} phone(s)."));
        }
        PushCommand::Test => {
            let sent = push::send_all(
                &path,
                &push::Relay::new(),
                &push::Message {
                    title: "Legio",
                    body: &format!("Notifications from {} work.", sys::hostname()),
                    level: push::Level::Active,
                    pane_id: None,
                },
            )?;
            info(&format!("Sent to {sent} phone(s)."));
        }
    }
    Ok(())
}

fn uninstall() -> anyhow::Result<()> {
    let home = sys::home()?;
    if cfg!(target_os = "macos") {
        mac::uninstall()?;
    } else {
        bold(&format!("Removing the {} units", bridge::UNIT_NAME));
        bridge::remove_units(&home);
        info(&format!(
            "Removed the units from {}.",
            bridge::unit_dir(&home).display()
        ));
    }

    bold("Removing the notification watcher");
    watch::remove(&home);
    let push_file = push::file(&home);
    if std::fs::remove_file(&push_file).is_ok() {
        info(&format!("Removed {}.", push_file.display()));
    }

    bold("Removing the paired phones");
    let file = keys::File::in_home(&home);
    let (text, removed) = keys::without(&file.read()?, |_, comment| {
        comment.starts_with(keys::MARKER)
    });
    if removed > 0 {
        file.write(&text)?;
    }
    info(&format!(
        "Removed {removed} phone key(s) from {}.",
        file.path.display()
    ));
    warn_joined(&text);

    if !cfg!(target_os = "macos") {
        let user = sys::user()?;
        info("Lingering is left on. Turn it off with:");
        info(&format!("  sudo loginctl disable-linger {user}"));
    }
    Ok(())
}
