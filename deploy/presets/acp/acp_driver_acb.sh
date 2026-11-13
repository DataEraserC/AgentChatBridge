#!/usr/bin/env bash
# acp_driver_acb.sh —— 通用 ACP 适配器的常驻对账（acp 组件的 acb 预设）。
#
# 一份驱动 = 一个 ACP agent 子进程 + 一条桥连接，按**会话**起：
#   acbctl sessions 里 harness 命中 ACB_ACP_HARNESSES 的会话 → 各起一个
#   `node harnesses/acp/src/driver.mjs`，token 用该会话自己的接入令牌。
#
# 为什么按会话而不是按 harness：令牌是会话级的（桥握手拿它绑定会话），一个进程
# 只能代表它拿到的令牌；dsh 的 drivers 组件也是这么做的。
#
# 启动命令本身不在这里写死：ACP 驱动按 ACB_ACP_AGENTS / ACB_ACP_COMMAND 解析，
# 这里只负责「哪个会话归哪个 harness」。于是 multi-instance、直接启动、包装脚本
# 都只是声明里的几个 argv token，换 agent 不用改这个脚本。
#
# 对账行为与 drivers_acb.sh 一致：
#   - 新会话：起驱动；
#   - 会话删除 / harness 不再命中：SIGTERM；
#   - token 或声明变了：只重启那一个；
#   - 单个驱动崩溃：指数退避重启，不影响别的会话。
set -uo pipefail

fail() { echo "acp: $*" >&2; exit 1; }
log() { printf '%s acp: %s\n' "$(date -Is)" "$*"; }

ONCE=0
for arg in "$@"; do
  case "$arg" in
    --once) ONCE=1 ;;
    *) fail "unknown argument: $arg" ;;
  esac
done

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PREFIX=$(cd "$HERE/../../../.." && pwd)
ACB_DATA=${ACB_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/acb}
CONFIG=${ACB_CONFIG_FILE:-$ACB_DATA/config/config.toml}
STATE_DIR=${ACB_ACP_STATE_DIR:-$ACB_DATA/state/acp}
READ_INTERVAL=${ACB_ACP_INTERVAL_SECS:-5}

NODE=${ACB_NODE_BIN:-}
[[ -n $NODE ]] || NODE=$(command -v node || true)
[[ -x $NODE ]] || fail "找不到 node（用 ACB_NODE_BIN 指定）"

# 驱动脚本：显式优先；否则按同目录推导（payload 布局 harnesses/{dsh,acp}/src）。
DRIVER=${ACB_ACP_DRIVER_SCRIPT:-}
if [[ -z $DRIVER && -n ${ACB_DRIVER_SCRIPT:-} ]]; then
  DRIVER=$(cd "$(dirname "$ACB_DRIVER_SCRIPT")/../../acp/src" && pwd)/driver.mjs
fi
[[ -n $DRIVER ]] || DRIVER=$PREFIX/harnesses/acp/src/driver.mjs
[[ -f $DRIVER ]] || fail "找不到 ACP 驱动：$DRIVER"

ACBCTL=${ACBCTL_BIN:-}
# 优先用**本次启动的 payload**里的 acbctl：PATH 上的可能是旧代（只认旧 schema）。
[[ -n $ACBCTL ]] || ACBCTL=$PREFIX/bin/acbctl
[[ -n $ACBCTL && -x $ACBCTL ]] || ACBCTL=$(command -v acbctl || true)
[[ -x $ACBCTL ]] || fail "找不到 acbctl（用 ACBCTL_BIN 指定）"

[[ -f $CONFIG ]] || fail "找不到 kernel 配置（用 ACB_CONFIG_FILE 指定）：$CONFIG"

# 这个组件负责哪些 harness：逗号/空白分隔。空 = 一个都不管（只打一行日志）。
ACP_HARNESSES=${ACB_ACP_HARNESSES:-}
mkdir -p "$STATE_DIR"

ACB_CWD=${ACB_ACP_CWD:-$ACB_DATA/agents}
mkdir -p "$ACB_CWD"

# 声明变了就该换进程：把声明原文与命令一起算进指纹，不用人肉重启。
AGENTS_FP=$(printf '%s' "${ACB_ACP_AGENTS:-}${ACB_ACP_COMMAND:-}" | sha256sum | cut -d' ' -f1)
DRIVER_FP=$(printf '%s' "$DRIVER" | sha256sum | cut -d' ' -f1)

declare -A pids fingerprints backoff next_try

cleanup() {
  trap - INT TERM EXIT
  for sid in "${!pids[@]}"; do
    kill "${pids[$sid]}" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM EXIT

matches() {
  local harness=$1 want
  [[ -n $ACP_HARNESSES ]] || return 1
  for want in ${ACP_HARNESSES//,/ }; do
    [[ $harness == "$want" ]] && return 0
  done
  return 1
}

session_token() {
  "$ACBCTL" secrets resolve --session "$1" --config "$CONFIG" 2>/dev/null
}

token_hash() {
  printf '%s' "$1" | sha256sum | cut -d' ' -f1
}

spawn_driver() {
  local sid=$1 harness=$2 token=$3
  local logfile=$STATE_DIR/$sid.log
  log "起驱动：会话 $sid（harness $harness）"
  env ACB_ENDPOINT="${ACB_ENDPOINT:-127.0.0.1:7700}" \
    ACB_TOKEN="$token" \
    ACB_HARNESS="$harness" \
    ACB_ACP_AGENT="$harness" \
    ACB_ACP_CWD="$ACB_CWD" \
    ACB_MCP_PORT=0 \
    "$NODE" "$DRIVER" >>"$logfile" 2>&1 &
  pids[$sid]=$!
  backoff[$sid]=0
  next_try[$sid]=0
}

stop_driver() {
  local sid=$1
  local pid=${pids[$sid]:-}
  [[ -n $pid ]] || return 0
  log "停驱动：会话 $sid（pid $pid）"
  kill "$pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
  done
  kill -9 "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  unset "pids[$sid]" "fingerprints[$sid]" "backoff[$sid]" "next_try[$sid]"
}

reconcile() {
  local output
  if ! output=$("$ACBCTL" sessions --config "$CONFIG" 2>>"$STATE_DIR/acbctl.err"); then
    log "acbctl sessions 失败；保留现有驱动，稍后重试"
    return 0
  fi

  local -A want_fp=() want_harness=() want_token=()
  local sid harness preset port token fp
  while IFS=$'\t' read -r sid harness preset port; do
    [[ -n $sid && -n $harness ]] || continue
    matches "$harness" || continue
    if ! token=$(session_token "$sid"); then
      log "会话 $sid 的 token 解析失败，保留现有驱动"
      continue
    fi
    if [[ -z $token ]]; then
      log "会话 $sid 的 token 为空，保留现有驱动"
      continue
    fi
    fp="$sid|$harness|$(token_hash "$token")|$AGENTS_FP|$DRIVER_FP|$ACB_CWD"
    want_fp[$sid]=$fp
    want_harness[$sid]=$harness
    want_token[$sid]=$token
  done <<<"$output"

  # 先停：被删除、或配置变了的会话。
  for sid in "${!pids[@]}"; do
    if [[ -z ${want_fp[$sid]:-} ]]; then
      stop_driver "$sid"
      continue
    fi
    if [[ ${fingerprints[$sid]:-} != "${want_fp[$sid]}" ]]; then
      log "会话 $sid 配置变化，重启驱动"
      stop_driver "$sid"
    fi
  done

  # 再起：新会话、或刚被停掉的。
  local now count delay
  now=$(date +%s)
  for sid in "${!want_fp[@]}"; do
    if [[ -n ${pids[$sid]:-} ]] && kill -0 "${pids[$sid]}" 2>/dev/null; then
      continue
    fi
    if [[ ${next_try[$sid]:-0} -gt $now ]]; then
      continue
    fi
    spawn_driver "$sid" "${want_harness[$sid]}" "${want_token[$sid]}"
    fingerprints[$sid]=${want_fp[$sid]}
  done

  # 检查崩溃：pid 不在了就记一次退避，下一轮重启。
  for sid in "${!pids[@]}"; do
    if ! kill -0 "${pids[$sid]}" 2>/dev/null; then
      count=${backoff[$sid]:-0}
      count=$((count + 1))
      backoff[$sid]=$count
      delay=$((1 << (count > 5 ? 5 : count)))
      [[ $delay -gt 60 ]] && delay=60
      next_try[$sid]=$((now + delay))
      log "驱动 $sid 退出（第 $count 次），${delay}s 后重试"
      unset "pids[$sid]" "fingerprints[$sid]"
    fi
  done
}

if [[ -z $ACP_HARNESSES ]]; then
  log "ACB_ACP_HARNESSES 为空：本组件不接管任何会话，只做空转"
fi

if [[ $ONCE -eq 1 ]]; then
  trap - INT TERM EXIT
  reconcile
  exit 0
fi

log "对账启动：配置 $CONFIG，harness ${ACP_HARNESSES:-（无）}，间隔 ${READ_INTERVAL}s，驱动 $DRIVER"
while true; do
  reconcile
  sleep "$READ_INTERVAL"
done
