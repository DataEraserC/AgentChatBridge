#!/usr/bin/env bash
# snowluma.sh —— SnowLuma 容器的唯一入口（旧命令已废弃）。
#
#   deploy/snowluma.sh single up      # 单账号形态
#   deploy/snowluma.sh multi  up      # 多账号形态
#   deploy/snowluma.sh multi  down
#   deploy/snowluma.sh multi  status
#   deploy/snowluma.sh multi  logs
#
# 为什么需要这个脚本：compose 文件在 ACB/deploy/，而真实端口值在
# ~/Documents/SnowLuma/.env。podman-compose 默认只读 compose 同目录的 .env，
# 直接手敲 -f 会读不到那些值、静默退回 3000/3001/6081/5099，从而切断 ACB
# （ACB 按 53000/53001 访问）。这里把 --env-file 固化，杜绝该事故。
#
# 两种形态共用容器名与三个登录态卷，是同一套栈的两种形态：
# 切换 = down 旧的 + up 新的，登录态不丢。不要同时 up 两份。
set -euo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ENV_FILE=${SNOWLUMA_ENV_FILE:-$HOME/Documents/SnowLuma/.env}
COMPOSE_BIN=${SNOWLUMA_COMPOSE_BIN:-podman-compose}

fail() { echo "snowluma: $*" >&2; exit 1; }

usage() {
  sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 1
}

[[ $# -ge 1 ]] || usage
MODE=$1; ACTION=${2:-up}

case $MODE in
  single) FILE=$HERE/snowluma-compose.single.yml ;;
  multi)  FILE=$HERE/snowluma-compose.multi.yml ;;
  -h|--help|help) usage ;;
  *) fail "未知形态：$MODE（只支持 single / multi）" ;;
esac

[[ -f $FILE ]] || fail "compose 文件不存在：$FILE"
command -v "$COMPOSE_BIN" >/dev/null 2>&1 || fail "找不到 compose 工具：$COMPOSE_BIN"

# env 文件缺失不静默继续：缺了就会用错端口，代价由 ACB 承担。
[[ -f $ENV_FILE ]] || fail "env 文件不存在：$ENV_FILE
真实端口值在该文件里（53000/53001/56081/55099）。
缺了会退回默认端口并切断 ACB，故此处直接失败。
可用 SNOWLUMA_ENV_FILE=<path> 指定其他位置。"

run() { "$COMPOSE_BIN" --env-file "$ENV_FILE" -f "$FILE" "$@"; }

case $ACTION in
  up)
    run up -d
    echo
    echo "── 现网端口自检（应看到 53000/53001）──"
    sleep 3
    if command -v ss >/dev/null 2>&1; then
      ss -ltn 2>/dev/null | grep -E '53000|53001' || echo "⚠️  尚未监听 53000/53001，容器可能还在启动，稍后重查"
    fi
    if [[ $MODE == multi ]]; then
      echo
      echo "── 多账号开关状态 ──"
      homes=$(grep -E '^SNOWLUMA_EXTRA_QQ_HOMES=' "$ENV_FILE" | tail -1 | cut -d= -f2- || true)
      if [[ -z ${homes//\"/} ]]; then
        echo "SNOWLUMA_EXTRA_QQ_HOMES 为空 → 当前仍是单账号运行。"
        echo "要加号：在 $ENV_FILE 写 SNOWLUMA_EXTRA_QQ_HOMES=/app/qq-acct2 后重跑 up。"
      else
        echo "SNOWLUMA_EXTRA_QQ_HOMES=$homes"
        echo "硬证据检查（应看到 qq-extra-N）："
        echo "  podman exec snowluma cat /etc/supervisor/conf.d/extra-qq.conf"
        echo "  podman exec snowluma supervisorctl status"
      fi
    fi
    ;;
  down)   run down ;;
  status) run ps ;;
  logs)   run logs -f ;;
  config) run config ;;
  *) fail "未知动作：$ACTION（支持 up / down / status / logs / config）" ;;
esac