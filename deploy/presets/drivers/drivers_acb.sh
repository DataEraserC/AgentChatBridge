#!/usr/bin/env bash
# drivers_acb.sh —— drivers 组件的 acb 预设脚本（搬自 deploy/start-drivers.sh）。
#
# 与旧 systemd 入口的差别：
#   1. 会话列表不再写死：`acbctl sessions` 从 kernel 配置逐行读
#      id / harness / preset / port（preset 与端口由同一套规则推导）；
#   2. 会话令牌仍走 `acbctl secrets resolve`，脚本里没有明文；
#   3. 任一驱动退出即整体退出（wait -n + 收掉其余进程），不再等所有会话都结束。
set -uo pipefail

fail() { echo "drivers: $*" >&2; exit 1; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# 安装布局：<prefix>/share/acb/presets/drivers/this.sh
PREFIX=$(cd "$HERE/../../../.." && pwd)
ACB_DATA=${ACB_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/acb}

NODE=${ACB_NODE_BIN:-}
[[ -n $NODE ]] || NODE=$(command -v node || true)
[[ -x $NODE ]] || fail "找不到 node（用 ACB_NODE_BIN 指定）"

DRIVER=${ACB_DRIVER_SCRIPT:-}
[[ -n $DRIVER ]] || fail "找不到 driver.mjs（用 ACB_DRIVER_SCRIPT 指定安装包里的绝对路径）"
[[ -f $DRIVER ]] || fail "找不到 driver.mjs（用 ACB_DRIVER_SCRIPT 指定）：$DRIVER"

ACBCTL=${ACBCTL_BIN:-}
[[ -n $ACBCTL ]] || ACBCTL=$(command -v acbctl || true)
[[ -n $ACBCTL && -x $ACBCTL ]] || ACBCTL=$PREFIX/bin/acbctl
[[ -x $ACBCTL ]] || fail "找不到 acbctl（用 ACBCTL_BIN 指定）"

CONFIG=${ACB_CONFIG_FILE:-$ACB_DATA/config/config.toml}
[[ -f $CONFIG ]] || fail "找不到 kernel 配置（用 ACB_CONFIG_FILE 指定）：$CONFIG"

PATCH=${ACB_DSH_PATCH:-}
SESSIONS_OUT=$("$ACBCTL" sessions --config "$CONFIG") || fail "acbctl sessions 失败"

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
  echo "模型：$DSH_PROVIDER/$DSH_MODEL${DSH_EFFORT:+（$DSH_EFFORT）}"
else
  echo "没有可读的 dsh patch，驱动不干预模型" >&2
fi

ACB_CWD="${ACB_CWD:-$ACB_DATA/agents}"
ACB_WORKSPACE_TITLE="${ACB_WORKSPACE_TITLE:-ACB}"
mkdir -p "$ACB_CWD"
export ACB_CWD ACB_WORKSPACE_TITLE
echo "dsh 会话归组：$ACB_CWD（workspace 标题：$ACB_WORKSPACE_TITLE）"

pids=()
while IFS=$'\t' read -r sid harness preset port; do
  [[ -n $sid ]] || continue
  [[ $harness == dsh ]] || continue
  token=$("$ACBCTL" secrets resolve --session "$sid" --config "$CONFIG") \
    || fail "解析不到会话 $sid 的 token"
  [[ -n $token ]] || fail "会话 $sid 的 token 为空"
  echo "起驱动：会话 $sid → MCP $port → preset $preset"
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
    "$NODE" "$DRIVER" &
  pids+=("$!")
done <<< "$SESSIONS_OUT"

[[ ${#pids[@]} -gt 0 ]] || fail "配置里没有 harness = dsh 的会话"

cleanup() {
  trap - INT TERM
  [[ ${#pids[@]} -gt 0 ]] && kill "${pids[@]}" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup INT TERM

wait -n "${pids[@]}"
status=$?
echo "有驱动退出（status=$status），收掉其余驱动" >&2
cleanup
exit "$status"
