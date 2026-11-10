#!/usr/bin/env bash
# drivers_acb.sh —— 会话驱动常驻对账（drivers 组件的 acb 预设）。
#
# 和旧版的一次性启动不同：这里每 READ_INTERVAL 秒读一次 `acbctl sessions`，
# 按会话 id 对账——
#   - 新会话：起一个 driver；
#   - 删除/禁用：SIGTERM 对应 driver；
#   - token / preset / port / model 变化：只重启那一个；
#   - 单个 driver 崩溃：指数退避重启，不再整组退出。
#
# 端口来自会话的显式 `driver_port`；旧配置没有时 acbctl 会按历史下标推导并告警。
# 状态只在内存里；进程重启后从当前会话清单重建。
set -uo pipefail

fail() { echo "drivers: $*" >&2; exit 1; }
log() { printf '%s drivers: %s\n' "$(date -Is)" "$*"; }

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
STATE_DIR=${ACB_DRIVERS_STATE_DIR:-$ACB_DATA/state/drivers}
READ_INTERVAL=${ACB_DRIVERS_INTERVAL_SECS:-5}

NODE=${ACB_NODE_BIN:-}
[[ -n $NODE ]] || NODE=$(command -v node || true)
[[ -x $NODE ]] || fail "找不到 node（用 ACB_NODE_BIN 指定）"

DRIVER=${ACB_DRIVER_SCRIPT:-}
[[ -n $DRIVER ]] || fail "找不到 driver.mjs（用 ACB_DRIVER_SCRIPT 指定）"
[[ -f $DRIVER ]] || fail "找不到 driver.mjs：$DRIVER"

ACBCTL=${ACBCTL_BIN:-}
# 优先用**本次启动的 payload**里的 acbctl：PATH 上的可能是旧代（只认旧 schema）。
[[ -n $ACBCTL ]] || ACBCTL=$PREFIX/bin/acbctl
[[ -n $ACBCTL && -x $ACBCTL ]] || ACBCTL=$(command -v acbctl || true)
[[ -x $ACBCTL ]] || fail "找不到 acbctl（用 ACBCTL_BIN 指定）"

[[ -f $CONFIG ]] || fail "找不到 kernel 配置（用 ACB_CONFIG_FILE 指定）：$CONFIG"

PATCH=${ACB_DSH_PATCH:-}
mkdir -p "$STATE_DIR"

# 模型：以 dsh patch 的 agent-default-model 为准；没给 patch 就不干预模型。
patch_model() {
  [[ -n $PATCH && -f $PATCH ]] || return 0
  awk -v key="$1" '
    /^- id: agent-default-model/ { f = 1; next }
    f && /^- / { f = 0 }
    f && $1 == key ":" { print $2; exit }
  ' "$PATCH"
}

DSH_PROVIDER=$(patch_model provider)
DSH_MODEL=$(patch_model model)
DSH_EFFORT=$(patch_model reasoningEffort)
if [[ -n $DSH_PROVIDER && -n $DSH_MODEL ]]; then
  log "模型：$DSH_PROVIDER/$DSH_MODEL${DSH_EFFORT:+（$DSH_EFFORT）}"
else
  log "没有可读的 dsh patch，驱动不干预模型"
fi

ACB_CWD="${ACB_CWD:-$ACB_DATA/agents}"
ACB_WORKSPACE_TITLE="${ACB_WORKSPACE_TITLE:-ACB}"
mkdir -p "$ACB_CWD"
export ACB_CWD ACB_WORKSPACE_TITLE

declare -A pids fingerprints backoff next_try

cleanup() {
  trap - INT TERM EXIT
  for sid in "${!pids[@]}"; do
    pid=${pids[$sid]}
    [[ -n $pid ]] || continue
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup INT TERM EXIT

session_token() {
  "$ACBCTL" secrets resolve --session "$1" --config "$CONFIG" 2>/dev/null
}

token_hash() {
  printf '%s' "$1" | sha256sum | cut -d' ' -f1
}

spawn_driver() {
  local sid=$1 preset=$2 port=$3 token=$4
  local logfile=$STATE_DIR/$sid.log
  log "起驱动：会话 $sid → MCP $port → preset $preset"
  env DSH_HOME="${DSH_HOME:-$HOME/.dsh}" \
    ACB_ENDPOINT="${ACB_ENDPOINT:-127.0.0.1:7700}" \
    ACB_TOKEN="$token" \
    ACB_HARNESS=dsh \
    ACB_PRESET="$preset" \
    ACB_MCP_PORT="$port" \
    DSH_BASE_URL="${DSH_BASE_URL:-http://127.0.0.1:3080}" \
    ACB_DSH_PROVIDER="$DSH_PROVIDER" \
    ACB_DSH_MODEL="$DSH_MODEL" \
    ACB_DSH_EFFORT="$DSH_EFFORT" \
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

  local -A want_fp=()
  local -A want_preset=()
  local -A want_port=()
  local -A want_token=()
  local sid harness preset port token fp

  while IFS=$'\t' read -r sid harness preset port; do
    [[ -n $sid && $harness == dsh ]] || continue
    [[ -n $preset && -n $port ]] || { log "会话 $sid 缺少 preset/port，跳过"; continue; }
    if ! token=$(session_token "$sid"); then
      log "会话 $sid 的 token 解析失败，保留现有驱动"
      continue
    fi
    if [[ -z $token ]]; then
      log "会话 $sid 的 token 为空，保留现有驱动"
      continue
    fi
    fp="$sid|$preset|$port|$(token_hash "$token")|${DSH_PROVIDER:-}/${DSH_MODEL:-}/${DSH_EFFORT:-}"
    want_fp[$sid]=$fp
    want_preset[$sid]=$preset
    want_port[$sid]=$port
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

  # 再起：新会话、或刚被停掉的会话。
  local now
  now=$(date +%s)
  for sid in "${!want_fp[@]}"; do
    # 进程还活着且指纹一致：不动。
    if [[ -n ${pids[$sid]:-} ]] && kill -0 "${pids[$sid]}" 2>/dev/null; then
      if [[ ${fingerprints[$sid]:-} == "${want_fp[$sid]}" ]]; then
        continue
      fi
      stop_driver "$sid"
    fi
    # 进程崩溃过：退避没到就等下一轮。
    if [[ ${next_try[$sid]:-0} -gt $now ]]; then
      continue
    fi
    spawn_driver "$sid" "${want_preset[$sid]}" "${want_port[$sid]}" "${want_token[$sid]}"
    fingerprints[$sid]=${want_fp[$sid]}
  done

  # 检查崩溃：pid 不在了就记一次退避，下一轮重启。
  for sid in "${!pids[@]}"; do
    if ! kill -0 "${pids[$sid]}" 2>/dev/null; then
      local count=${backoff[$sid]:-0}
      count=$((count + 1))
      backoff[$sid]=$count
      local delay=$((1 << (count > 5 ? 5 : count)))
      [[ $delay -gt 60 ]] && delay=60
      next_try[$sid]=$((now + delay))
      log "驱动 $sid 退出（第 $count 次），${delay}s 后重试"
      unset "pids[$sid]" "fingerprints[$sid]"
    fi
  done
}

if [[ $ONCE -eq 1 ]]; then
  # 一次性模式用于测试/巡检：拉起后不回收，便于调用方检查进程与端口。
  trap - INT TERM EXIT
  reconcile
  exit 0
fi

log "对账启动：配置 $CONFIG，间隔 ${READ_INTERVAL}s"
while true; do
  reconcile
  sleep "$READ_INTERVAL"
done
