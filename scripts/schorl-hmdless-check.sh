#!/usr/bin/env bash
# HMD 無しで繋ぎ目を実測するための走らせ方。
#
# `pin verify.machine_scope` の五点のうち、機械だけで言えるところを走らせる。
# **HMD を被った受け入れはここに含まれない** (`pin verify.hmd_gate` /
# `pin verify.no_green_substitute`)。
#
# 使い方:
#   scripts/schorl-hmdless-check.sh                # 既定の検査を走らせる
#   scripts/schorl-hmdless-check.sh <cmd> [args…]  # 任意の物を同じ座で走らせる
#
# このホストでの実測事実 (2026-09-23):
#   - monado-service は stdin に pipe が要る。/dev/null を与えると
#     epoll_ctl(stdin) failed で落ちる。
#   - 既存の Hyprland セッション (WAYLAND_DISPLAY=wayland-1) の上へ出す。
#     宿主のセッションは置き換えない (`pin wm.host_compositor_coexistence`)。
#
# 起こした物は必ず返す (`pin host.created_resource_lifecycle`):
#   monado-service / stdin を開けておく保持プロセス /
#   $XDG_RUNTIME_DIR の monado.pid と monado_comp_ipc。
set -u

: "${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}"
: "${SCHORL_HOST_WAYLAND_DISPLAY:=wayland-1}"

here="$(cd "$(dirname "$0")/.." && pwd)"
log_dir="${SCHORL_CHECK_LOG_DIR:-${TMPDIR:-/tmp}/schorl-hmdless-check}"
mkdir -p "$log_dir"
fifo="$log_dir/monado-stdin"
monado_log="$log_dir/monado.log"

monado_pid=""
holder_pid=""

cleanup() {
  if [ -n "$monado_pid" ] && kill -0 "$monado_pid" 2>/dev/null; then
    kill -TERM "$monado_pid" 2>/dev/null
    for _ in $(seq 1 40); do
      kill -0 "$monado_pid" 2>/dev/null || break
      sleep 0.25
    done
    kill -KILL "$monado_pid" 2>/dev/null
    wait "$monado_pid" 2>/dev/null
  fi
  if [ -n "$holder_pid" ] && kill -0 "$holder_pid" 2>/dev/null; then
    kill -KILL "$holder_pid" 2>/dev/null
    wait "$holder_pid" 2>/dev/null
  fi
  rm -f "$fifo"
  # monado が残す物はここで返す。次の走りが古い座を掴まないようにする。
  rm -f "$XDG_RUNTIME_DIR/monado.pid" "$XDG_RUNTIME_DIR/monado_comp_ipc"
}
trap cleanup EXIT INT TERM

rm -f "$fifo"
mkfifo "$fifo"
# stdin を開けたままにする保持プロセス。pipe が閉じると monado は落ちる。
sleep 100000 > "$fifo" &
holder_pid=$!

WAYLAND_DISPLAY="$SCHORL_HOST_WAYLAND_DISPLAY" QWERTY_ENABLE=1 \
  monado-service < "$fifo" > "$monado_log" 2>&1 &
monado_pid=$!

ready=0
for _ in $(seq 1 80); do
  if [ -S "$XDG_RUNTIME_DIR/monado_comp_ipc" ]; then ready=1; break; fi
  kill -0 "$monado_pid" 2>/dev/null || break
  sleep 0.25
done
if [ "$ready" -ne 1 ]; then
  echo "monado-service did not open $XDG_RUNTIME_DIR/monado_comp_ipc; see $monado_log" >&2
  tail -20 "$monado_log" >&2
  exit 70
fi
echo "# monado-service is up (pid $monado_pid, log $monado_log)" >&2

if [ "$#" -gt 0 ]; then
  "$@"
else
  "$here/target/debug/schorl-seam-check"
fi
status=$?

echo "# check exited $status" >&2
exit "$status"
