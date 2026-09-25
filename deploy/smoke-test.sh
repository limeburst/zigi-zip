# Runs on the server, appended to the remote scripts of deploy.sh and
# rollback.sh. Expects DATA and CONFIG to be set.
#
# Smoke-test once it is up. macOS kills the first exec of a freshly signed
# binary (OS_REASON_CODESIGNING) and KeepAlive restarts it, so a single
# one-second check races that restart and reports a healthy deploy as failed.
LISTEN="$(awk -F'"' '/^listen/ {print $2}' "$CONFIG")"
code=000
for _ in $(seq 30); do
    sleep 1
    code=$(curl -s -o /dev/null -w "%{http_code}" "http://$LISTEN/" || true)
    [[ "$code" == "200" ]] && break
done
echo "==> GET / -> $code"
if [[ "$code" != "200" ]]; then
    echo "service did not come back up; check $DATA/zigi.err.log" >&2
    exit 1
fi
