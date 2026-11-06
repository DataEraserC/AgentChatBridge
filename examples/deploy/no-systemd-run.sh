#!/usr/bin/env bash
# 无 systemd：用 acbctl run 的前台父循环；run.json 就是 pidfile。
# 脚本只把白名单 ACB_UNIT_* 传给 acbctl，env 文件里的其它变量不会进入子进程。
#
# 用法：
#   examples/deploy/no-systemd-run.sh
#   ACBCTL_BIN=/path/to/acbctl examples/deploy/no-systemd-run.sh
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)

ENV_FILE=${ACB_SOURCE_ENV:-$REPO/deploy/acb.env}
set -a
if [[ -f $ENV_FILE ]]; then
  # shellcheck disable=SC1090
  . "$ENV_FILE"
else
  echo "warning: env file not found: $ENV_FILE" >&2
fi
set +a

: "${ACB_UNIT_REPO_DIR:=$REPO}"
: "${ACB_UNIT_LOG_DIR:=$REPO/state/logs}"
: "${ACB_UNIT_PROFILE_DIR:=/etc/profiles/per-user/$(id -un)}"
: "${ACB_UNIT_BIN_DIR:=$REPO/result/bin}"
: "${ACB_UNIT_CONFIG_FILE:=$REPO/config.toml}"
: "${ACB_UNIT_ENV_FILE:=$REPO/deploy/acb.env}"
: "${ACB_UNIT_OPENCODE_DIR:=$REPO/state/opencode-1093015329}"
: "${ACB_UNIT_CONSOLE_PORT:=3100}"
: "${ACB_UNIT_OPENCODE_PORT:=4097}"
: "${ACB_UNIT_AUTOSTART:=0}"

UNIT_KEYS=(
  ACB_UNIT_REPO_DIR
  ACB_UNIT_LOG_DIR
  ACB_UNIT_PROFILE_DIR
  ACB_UNIT_BIN_DIR
  ACB_UNIT_CONFIG_FILE
  ACB_UNIT_ENV_FILE
  ACB_UNIT_OPENCODE_DIR
  ACB_UNIT_CONSOLE_PORT
  ACB_UNIT_OPENCODE_PORT
  ACB_UNIT_AUTOSTART
)

EXPORTS=()
for key in "${UNIT_KEYS[@]}"; do
  if [[ -z ${!key:-} ]]; then
    echo "missing required variable: $key" >&2
    exit 2
  fi
  EXPORTS+=("$key=${!key}")
done

ACBCTL_BIN=${ACBCTL_BIN:-}
if [[ -z $ACBCTL_BIN ]]; then
  for candidate in "$REPO/target/release/acbctl" "$REPO/target/debug/acbctl"; do
    if [[ -x $candidate ]]; then
      ACBCTL_BIN=$candidate
      break
    fi
  done
fi
if [[ -z $ACBCTL_BIN ]]; then
  ACBCTL_BIN=$(command -v acbctl || true)
fi
if [[ -z $ACBCTL_BIN || ! -x $ACBCTL_BIN ]]; then
  echo "acbctl binary not found; build crates/acb or set ACBCTL_BIN" >&2
  exit 2
fi
if ! "$ACBCTL_BIN" unit --help >/dev/null 2>&1; then
  echo "$ACBCTL_BIN does not support 'unit'; build crates/acb or set ACBCTL_BIN" >&2
  exit 2
fi

STATE_DIR=${ACB_RUN_STATE_DIR:-$ACB_UNIT_REPO_DIR/state/supervisor}
mkdir -p "$STATE_DIR"

# acbctl run 自己是前台循环；这里放到后台只是为了演示同时查询 pidfile。
env -i "${EXPORTS[@]}" "$ACBCTL_BIN" run --mode=launcher --state-dir "$STATE_DIR" &
run_pid=$!
trap 'kill -TERM "$run_pid" 2>/dev/null || true' INT TERM

for _ in $(seq 1 50); do
  [[ -f $STATE_DIR/run.json ]] && break
  sleep 0.1
done

# status --mode=run 只读 run.json/current.json/lkg.json + HTTP，不调用 systemctl。
env -i "${EXPORTS[@]}" "$ACBCTL_BIN" status --mode=run --state-dir "$STATE_DIR" || true

wait "$run_pid"
