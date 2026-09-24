#!/usr/bin/env bash
# Build locally, ship the binary to the mac mini server, migrate, and restart
# the zigi service. Run from a checkout on your dev machine.
#
#   deploy/deploy.sh
#
# DEPLOY_HOST overrides the ssh destination (user@host or a ~/.ssh/config alias).
# The server's data dir defaults to ~/zigi-data; override with REMOTE_DATA.

set -euo pipefail

HOST="${DEPLOY_HOST:-zigi-zip-deploy}" # alias in ~/.ssh/config
REMOTE_DATA="${REMOTE_DATA:-zigi-data}" # relative paths resolve against the remote $HOME
SRC="$(cd "$(dirname "$0")/.." && pwd)"

# The binary runs natively on the server, so the local build target must match.
local_platform="$(uname -sm)"
remote_platform="$(ssh "$HOST" uname -sm)"
if [[ "$local_platform" != "$remote_platform" ]]; then
    echo "local platform ($local_platform) does not match server ($remote_platform)" >&2
    exit 1
fi

echo "==> building release binary"
cd "$SRC"
cargo build --release

echo "==> copying binary to $HOST"
scp -q "$SRC/target/release/zigi-zip" "$HOST:$REMOTE_DATA/bin/zigi-zip.new"

ssh "$HOST" bash -s -- "$REMOTE_DATA" <<'REMOTE'
set -euo pipefail
cd "$HOME"
DATA="$1"
BIN="$DATA/bin/zigi-zip"
CONFIG="$DATA/config.toml"
LABEL="zip.zigi"

echo "==> installing binary"
chmod 755 "$BIN.new"
# macOS kills linker-signed binaries after a plain copy — re-sign ad-hoc, then
# swap it in with a rename so the running service never sees a partial file.
codesign --force --sign - "$BIN.new"
mv -f "$BIN.new" "$BIN"

echo "==> running migrations"
"$BIN" --config "$CONFIG" migrate

echo "==> restarting launchd agent"
launchctl kickstart -k "gui/$(id -u)/$LABEL"

# Smoke-test once it is up. macOS kills the first exec of a freshly signed
# binary (OS_REASON_CODESIGNING) and KeepAlive restarts it, so a single
# one-second check races that restart and reports a healthy deploy as failed.
LISTEN="$(awk -F'"' '/^listen/ {print $2}' "$CONFIG")"
code=000
for _ in $(seq 30); do
    sleep 1
    code=$(curl -sS -o /dev/null -w "%{http_code}" "http://$LISTEN/" || echo 000)
    [[ "$code" == "200" ]] && break
done
echo "==> GET / -> $code"
if [[ "$code" != "200" ]]; then
    echo "service did not come back up; check $DATA/zigi.err.log" >&2
    exit 1
fi
REMOTE

echo "==> done"
