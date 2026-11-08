#!/usr/bin/env bash
# snowluma_container.sh —— snowluma 组件的 container 预设脚本（oneshot）。
#
# 正常路径由管理内核直接拼 `compose_bin -f file up -d`；本脚本是等价兜底，
# 用于把预设脚本显式写进 component.command 的部署。file 与 compose_bin 都来自
# 配置（或管理内核注入的 ACB_COMPONENT_FILE / ACB_COMPONENT_COMPOSE_BIN_*），
# 这里不写死任何路径；缺了就失败，绝不猜。
set -euo pipefail

fail() { echo "snowluma: $*" >&2; exit 1; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

FILE=${ACB_COMPONENT_FILE:-${ACB_SNOWLUMA_FILE:-}}
[[ -n $FILE ]] || fail "没有 compose 文件（在组件配置里声明 file）"
[[ -f $FILE ]] || fail "compose 文件不存在：$FILE"

COMPOSE_BIN=${ACB_COMPONENT_COMPOSE_BIN_0:-}
[[ -n $COMPOSE_BIN ]] || fail "没有 compose 工具（在组件配置里声明 compose_bin）"
command -v "$COMPOSE_BIN" >/dev/null 2>&1 || [[ -x $COMPOSE_BIN ]] \
  || fail "找不到 compose 工具：$COMPOSE_BIN"

exec "$COMPOSE_BIN" -f "$FILE" up -d
