#!/usr/bin/env bash
# Usage: capture.sh [WxH] [wait_seconds] [key|resize=WxH ...]
# Runs mtr-ng (--simulate unless MTR_REAL=1) in a detached tmux session, sends keys, prints the pane, cleans up.
set -euo pipefail

size=${1:-120x30}
wait=${2:-3}
shift $(( $# < 2 ? $# : 2 ))

repo=$(cd "$(dirname "$0")/../../.." && pwd)
bin=${MTR_BIN:-$repo/target/debug/mtr-ng}
[ -x "$bin" ] || { echo "binary not found: $bin (run cargo build)" >&2; exit 1; }

workdir=$(mktemp -d)
session=mtrchk-$$
trap 'tmux kill-session -t "$session" 2>/dev/null || true; rm -rf "$workdir"' EXIT

mode=--simulate
[ "${MTR_REAL:-0}" = 1 ] && mode=
tmux new-session -d -s "$session" -x "${size%x*}" -y "${size#*x}" -c "$workdir" \
  "$bin $mode ${MTR_ARGS:-} ${MTR_TARGET:-example.com}; echo '[exited]'; sleep 30"
sleep "$wait"

for key in "$@"; do
  case $key in
    resize=*) dims=${key#resize=}; tmux resize-window -t "$session" -x "${dims%x*}" -y "${dims#*x}" ;;
    *) tmux send-keys -t "$session" "$key" ;;
  esac
  sleep 0.5
done

tmux capture-pane -p -t "$session"
