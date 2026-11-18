#!/bin/sh
# The staging directory is on the same filesystem as the installed application.
set -eu
pid=$1
target=$2
staged=$3
stage=$4
launch=$5
backup="$stage/previous"
ack="$stage/started"

while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
if ! mv "$target" "$backup"; then
  ZP_UPDATE_ROLLBACK="$stage" "$launch" > "$stage/rollback.log" 2>&1 &
  exit 1
fi
if ! mv "$staged" "$target"; then
  mv "$backup" "$target"
  ZP_UPDATE_ROLLBACK="$stage" "$launch" > "$stage/rollback.log" 2>&1 &
  exit 1
fi
ZP_UPDATE_ACK="$ack" "$launch" > "$stage/launch.log" 2>&1 &
child=$!
attempt=0
while [ "$attempt" -lt 150 ]; do
  if [ -f "$ack" ]; then
    # The new version is running: cleanup is best effort.
    rm -rf "$stage" || true
    exit 0
  fi
  if ! kill -0 "$child" 2>/dev/null; then break; fi
  attempt=$((attempt + 1))
  sleep 0.2
done
kill "$child" 2>/dev/null || true
sleep 0.2
kill -9 "$child" 2>/dev/null || true
wait "$child" 2>/dev/null || true
mv "$target" "$stage/failed"
mv "$backup" "$target"
ZP_UPDATE_ROLLBACK="$stage" "$launch" >> "$stage/rollback.log" 2>&1 &
