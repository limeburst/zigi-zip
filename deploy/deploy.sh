#!/usr/bin/env bash
# Rebuild, migrate, and restart the zigi service. Run on the mac mini server.
# Assumes source is at ~/Git/zigi-zip and data dir is ~/zigi-data.

set -euo pipefail

SRC="${SRC:-$HOME/Git/zigi-zip}"
DATA="${DATA:-$HOME/zigi-data}"
BIN="$DATA/bin/zigi-zip"
CONFIG="$DATA/config.toml"
PG_BIN="/opt/homebrew/opt/postgresql@18/bin"
LABEL="zip.zigi"

echo "==> building release binary"
cd "$SRC"
PATH="$PG_BIN:$PATH" cargo build --release

echo "==> installing binary"
install -m 755 "$SRC/target/release/zigi-zip" "$BIN"
# macOS kills linker-signed binaries after a plain cp — re-sign ad-hoc.
codesign --force --sign - "$BIN"

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
echo "==> done"
