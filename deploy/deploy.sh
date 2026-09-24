#!/usr/bin/env bash
# Build locally, ship the binary and configuration to the mac mini server,
# migrate, and restart the zigi service. Run from a checkout on your dev machine.
#
#   deploy/deploy.sh
#
# This machine is the source of truth for configuration; every deploy
# overwrites the server's copies:
#   - config.toml from $CONFIG_DIR (default ~/.config/zigi-zip/production),
#     kept out of the repo because it holds secrets
#   - the launchd agent from deploy/zip.zigi.plist
#
# DEPLOY_HOST overrides the ssh destination (user@host or a ~/.ssh/config alias).
# The server's data dir defaults to ~/zigi-data; override with REMOTE_DATA.

set -euo pipefail

HOST="${DEPLOY_HOST:-zigi-zip-deploy}" # alias in ~/.ssh/config
REMOTE_DATA="${REMOTE_DATA:-zigi-data}" # relative paths resolve against the remote $HOME
CONFIG_DIR="${CONFIG_DIR:-$HOME/.config/zigi-zip/production}"
SRC="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="$REMOTE_DATA/.deploy"

if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
    echo "missing $CONFIG_DIR/config.toml (see deploy/config.example.toml)" >&2
    exit 1
fi

# The binary runs natively on the server, so the local build target must match.
local_platform="$(uname -sm)"
remote_platform="$(ssh "$HOST" "uname -sm && mkdir -p '$STAGE'")"
if [[ "$local_platform" != "$remote_platform" ]]; then
    echo "local platform ($local_platform) does not match server ($remote_platform)" >&2
    exit 1
fi

echo "==> building release binary"
cd "$SRC"
cargo build --release

echo "==> copying binary and configuration to $HOST"
scp -q \
    "$SRC/target/release/zigi-zip" \
    "$CONFIG_DIR/config.toml" \
    "$SRC/deploy/zip.zigi.plist" \
    "$HOST:$STAGE/"

ssh "$HOST" bash -s -- "$REMOTE_DATA" <<'REMOTE'
set -euo pipefail
cd "$HOME"
DATA="$1"
STAGE="$DATA/.deploy"
BIN="$DATA/bin/zigi-zip"
CONFIG="$DATA/config.toml"
LABEL="zip.zigi"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

# Report only whether a file changed, never a diff: config.toml holds secrets.
install_file() { # staged-name destination mode
    local src="$STAGE/$1" dst="$2"
    chmod "$3" "$src"
    if cmp -s "$src" "$dst"; then
        rm "$src"
        echo "    $dst unchanged"
        return 1
    fi
    mv -f "$src" "$dst"
    echo "    $dst updated"
}

echo "==> installing configuration"
install_file config.toml "$CONFIG" 600 || true
plist_changed=0
install_file zip.zigi.plist "$PLIST" 644 && plist_changed=1

echo "==> installing binary"
chmod 755 "$STAGE/zigi-zip"
# macOS kills linker-signed binaries after a plain copy — re-sign ad-hoc, then
# swap it in with a rename so the running service never sees a partial file.
codesign --force --sign - "$STAGE/zigi-zip"
mv -f "$STAGE/zigi-zip" "$BIN"

echo "==> running migrations"
"$BIN" --config "$CONFIG" migrate

if [[ "$plist_changed" == 1 ]]; then
    # kickstart keeps the loaded job definition, so a new plist needs a reload.
    echo "==> reloading launchd agent"
    launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
    for _ in $(seq 10); do
        launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
        sleep 1
    done
    launchctl bootstrap "$DOMAIN" "$PLIST"
else
    echo "==> restarting launchd agent"
    launchctl kickstart -k "$DOMAIN/$LABEL"
fi

# Smoke-test once it is up. macOS kills the first exec of a freshly signed
# binary (OS_REASON_CODESIGNING) and KeepAlive restarts it, so a single
# one-second check races that restart and reports a healthy deploy as failed.
LISTEN="$(awk -F'"' '/^listen/ {print $2}' "$CONFIG")"
code=000
for _ in $(seq 30); do
    sleep 1
    code=$(curl -sS -o /dev/null -w "%{http_code}" "http://$LISTEN/" 2>/dev/null || echo 000)
    [[ "$code" == "200" ]] && break
done
echo "==> GET / -> $code"
if [[ "$code" != "200" ]]; then
    echo "service did not come back up; check $DATA/zigi.err.log" >&2
    exit 1
fi
REMOTE

echo "==> done"
