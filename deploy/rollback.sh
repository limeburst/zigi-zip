#!/usr/bin/env bash
# Swap the previously deployed binary back in and restart the zigi service.
# Run from a checkout on your dev machine.
#
#   deploy/rollback.sh
#
# The two binaries trade places, so running it again undoes the rollback.
# Only the binary rolls back: configuration stays as last deployed, and
# database migrations are not reverted, so the previous binary must still work
# against the current schema.
#
# DEPLOY_HOST and REMOTE_DATA work as in deploy.sh.

set -euo pipefail

HOST="${DEPLOY_HOST:-zigi-zip-deploy}" # alias in ~/.ssh/config
REMOTE_DATA="${REMOTE_DATA:-zigi-data}" # relative paths resolve against the remote $HOME
SRC="$(cd "$(dirname "$0")/.." && pwd)"

{ cat <<'REMOTE'; cat "$SRC/deploy/smoke-test.sh"; } | ssh "$HOST" bash -s -- "$REMOTE_DATA"
set -euo pipefail
cd "$HOME"
DATA="$1"
BIN="$DATA/bin/zigi-zip"
CONFIG="$DATA/config.toml"
LABEL="zip.zigi"

if [[ ! -f "$BIN.prev" ]]; then
    echo "no previous binary at $BIN.prev to roll back to" >&2
    exit 1
fi

echo "==> swapping in the previous binary"
# Renames only, so $BIN always exists for KeepAlive to exec.
ln -f "$BIN" "$BIN.swap"
mv -f "$BIN.prev" "$BIN"
mv -f "$BIN.swap" "$BIN.prev"

echo "==> restarting launchd agent"
launchctl kickstart -k "gui/$(id -u)/$LABEL"
REMOTE

echo "==> done"
