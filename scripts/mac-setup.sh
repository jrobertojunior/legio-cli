#!/usr/bin/env bash
#
# mac-setup.sh — install the bridge on a Mac, for the Legio iOS app.
#
# `legio` carries this script in its binary and runs it on macOS, in place
# of the systemd units it writes on Linux: `legio setup` installs with it,
# and `legio uninstall` removes with it. Run `legio`, not this file.
#
# The app makes one SSH connection to this Mac. Over that connection it
# opens a direct-tcpip channel to 127.0.0.1:<target port>, where socat
# exposes Herdr's Unix socket as a TCP port. The app also opens a PTY on the
# same connection and types `herdr session attach <session>`.
#
# This script installs the bridge. It does three things:
#   1. Checks Remote Login, and installs socat with Homebrew.
#   2. Checks that the Herdr socket exists.
#   3. Installs a LaunchAgent that keeps the TCP port open across reboots,
#      then asks the running Herdr whether it is new enough for the app.
#
# `legio setup` then checks the SSH server and pairs the phone. The phone
# makes its own key and sends only the public half; this script makes no key.
#
# Arguments, which `legio` passes:
#   [--port 4499] [--socket PATH] [--session default]
#   [--on-demand] [--uninstall]
#
# macOS has no bundled socket proxy, unlike systemd's systemd-socket-proxyd,
# so socat is needed here. `brew install socat` is the only new package.

set -euo pipefail

TARGET_PORT="4499"
HERDR_SOCKET="${HOME}/.config/herdr/herdr.sock"
SESSION_NAME="default"
ON_DEMAND="no"
UNINSTALL="no"

LABEL="com.legio.bridge"
AGENT_DIR="${HOME}/Library/LaunchAgents"
AGENT_PLIST="${AGENT_DIR}/${LABEL}.plist"
LOG_FILE="${HOME}/Library/Logs/${LABEL}.log"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
warn() { printf '\033[33mWARNING: %s\033[0m\n' "$*"; }
fail() { printf '\033[31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --port)    TARGET_PORT="${2:?--port needs a value}"; shift 2 ;;
        --socket)  HERDR_SOCKET="${2:?--socket needs a value}"; shift 2 ;;
        --session) SESSION_NAME="${2:?--session needs a value}"; shift 2 ;;
        --on-demand) ON_DEMAND="yes"; shift ;;
        --uninstall) UNINSTALL="yes"; shift ;;
        *) fail "unknown option: $1" ;;
    esac
done

case "$TARGET_PORT" in
    ''|*[!0-9]*) fail "--port must be a number: $TARGET_PORT" ;;
esac

GUI_TARGET="gui/$(id -u)"

# The app was called HerdrPOC before, and the agent had the label below.
# That agent holds the same port, so remove it before the new one starts.
OLD_LABEL="com.herdrpoc.bridge"
remove_old_agent() {
    launchctl bootout "${GUI_TARGET}/${OLD_LABEL}" 2>/dev/null || true
    rm -f "${AGENT_DIR}/${OLD_LABEL}.plist"
}

# ---------------------------------------------------------------- uninstall

if [ "$UNINSTALL" = "yes" ]; then
    bold "Removing the ${LABEL} LaunchAgent"
    launchctl bootout "${GUI_TARGET}/${LABEL}" 2>/dev/null || true
    rm -f "$AGENT_PLIST"
    remove_old_agent
    info "Removed ${AGENT_PLIST}."
    info "Remote Login is left on. Turn it off in System Settings > General > Sharing."
    exit 0
fi

# ------------------------------------------------------------- requirements

bold "1. Checking the requirements"

if nc -z -w1 127.0.0.1 22 >/dev/null 2>&1; then
    info "Remote Login is on. The SSH server answers on port 22."
else
    warn "Nothing answers on port 22. Turn on Remote Login:"
    warn "  System Settings > General > Sharing > Remote Login"
    warn "The rest of this script still runs, but the app cannot connect yet."
fi

command -v brew >/dev/null 2>&1 || fail "Homebrew not found. See https://brew.sh"

if command -v socat >/dev/null 2>&1; then
    info "socat: $(command -v socat)"
else
    info "socat is missing. Installing it with Homebrew."
    brew install socat
fi
command -v socat >/dev/null 2>&1 || fail "socat is still missing after the install."
SOCAT_BIN="$(command -v socat)"

if command -v herdr >/dev/null 2>&1; then
    info "herdr: $(command -v herdr)"
    info "Version: $(herdr --version 2>/dev/null | sed "s/^herdr //")"
    info "Update channel: $(herdr channel show 2>/dev/null || echo unknown)"
else
    warn "herdr is not on PATH for this shell."
    warn "The app types 'herdr session attach ${SESSION_NAME}' in a login shell."
    warn "Put herdr on the PATH of your login shell before you attach to a pane."
fi

# ------------------------------------------------------------------- socket

bold "2. Checking the Herdr socket"

if [ -S "$HERDR_SOCKET" ]; then
    info "Socket: ${HERDR_SOCKET}"
else
    warn "No socket at ${HERDR_SOCKET}."
    warn "Start the Herdr server first. The TCP port opens anyway, but the"
    warn "app cannot connect until the socket exists."
fi

# ------------------------------------------------------------- launchagent

bold "3. Installing the LaunchAgent"

mkdir -p "$AGENT_DIR"
launchctl bootout "${GUI_TARGET}/${LABEL}" 2>/dev/null || true
remove_old_agent

if [ "$ON_DEMAND" = "yes" ]; then
    # launchd holds the listening socket and hands socat one accepted
    # connection on stdio, the inetd shape. It fits Herdr, which closes a
    # connection after one request, and leaves no resident proxy process.
    cat > "$AGENT_PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>${LABEL}</string>
	<key>inetdCompatibility</key>
	<dict>
		<key>Wait</key>
		<false/>
	</dict>
	<key>Sockets</key>
	<dict>
		<key>Listeners</key>
		<dict>
			<key>SockNodeName</key>
			<string>127.0.0.1</string>
			<key>SockServiceName</key>
			<string>${TARGET_PORT}</string>
			<key>SockType</key>
			<string>stream</string>
			<key>SockFamily</key>
			<string>IPv4</string>
		</dict>
	</dict>
	<key>ProgramArguments</key>
	<array>
		<string>${SOCAT_BIN}</string>
		<string>STDIO</string>
		<string>UNIX-CONNECT:${HERDR_SOCKET}</string>
	</array>
	<key>StandardErrorPath</key>
	<string>${LOG_FILE}</string>
</dict>
</plist>
PLIST_EOF
    info "Wrote ${AGENT_PLIST} (on demand, one socat per connection)."
else
    # The default. socat owns the listening port itself, which is the exact
    # command the README documents — the shape most likely to work first
    # time. KeepAlive restarts it if it dies.
    cat > "$AGENT_PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>${LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>${SOCAT_BIN}</string>
		<string>TCP-LISTEN:${TARGET_PORT},bind=127.0.0.1,reuseaddr,fork</string>
		<string>UNIX-CONNECT:${HERDR_SOCKET}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardErrorPath</key>
	<string>${LOG_FILE}</string>
</dict>
</plist>
PLIST_EOF
    info "Wrote ${AGENT_PLIST} (always on)."
fi

launchctl bootstrap "$GUI_TARGET" "$AGENT_PLIST"
launchctl enable "${GUI_TARGET}/${LABEL}" 2>/dev/null || true

sleep 1
if nc -z -w1 127.0.0.1 "$TARGET_PORT" >/dev/null 2>&1; then
    info "The bridge answers on 127.0.0.1:${TARGET_PORT}."
else
    warn "Nothing answers on 127.0.0.1:${TARGET_PORT} yet. Read the log:"
    warn "  cat ${LOG_FILE}"
    warn "  launchctl print ${GUI_TARGET}/${LABEL}"
fi

# Asks the running Herdr whether it knows `agent.kinds`, the one method the
# app needs that older builds do not have. Without it the app's new-tab
# sheet cannot list the agent harnesses, and says so.
#
# Deliberately not a version-number check: the number cannot tell the two
# builds apart. The build that answers `agent.kinds` and the build that
# rejects it both call themselves 0.9.0 — the method arrived on the preview
# channel, not in a new version number. The method is the fact, so the
# method is what this asks.
#
# Spoken over the bridge this script just installed, in plain bash, so it
# needs no extra tool. Herdr serves one request per connection, so one
# connection is all this opens.
probe_agent_kinds() {
    local reply=""
    # The brace group catches bash's own "Connection refused" line, which a
    # redirect on `exec` alone does not silence.
    { exec 3<>"/dev/tcp/127.0.0.1/${TARGET_PORT}"; } 2>/dev/null || {
        warn "Could not open 127.0.0.1:${TARGET_PORT}, so the Herdr version"
        warn "stays unchecked."
        return 0
    }
    printf '{"id":"setup","method":"agent.kinds","params":{}}\n' >&3
    IFS= read -r -t 5 reply <&3 || true
    exec 3<&- 3>&- || true

    case "$reply" in
        *'"result"'*)
            info "Herdr answers agent.kinds. The app can list agent harnesses."
            ;;
        "")
            warn "Herdr did not answer within 5 seconds. Make sure the server"
            warn "is running, then run legio setup again."
            ;;
        *"unknown variant"*|*'"error"'*)
            warn "This Herdr does not know the 'agent.kinds' method."
            warn "Everything else works. Only the new-tab sheet is affected:"
            warn "it cannot list the agent harnesses, so it offers a plain"
            warn "shell and says 'Could not list agents'."
            warn "The method is on the preview channel. To get it:"
            warn "  herdr channel set preview"
            warn "  herdr update"
            warn "  herdr server stop     # the running server keeps the old binary"
            warn "Then start Herdr again and run legio setup again."
            ;;
        *)
            warn "Herdr gave an answer this script does not recognise:"
            warn "  ${reply}"
            ;;
    esac
}

probe_agent_kinds
