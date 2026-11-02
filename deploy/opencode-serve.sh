#!/usr/bin/env bash
# opencode-serve.sh —— 新的常驻方案：systemd 直接跑 opencode2 serve。
#
# 旧的 opencode-keeper.sh 靠 `opencode2 run` 踢一脚，让临时进程加载插件再连桥；
# run 退出后连接就断，还得靠 keeper 反复踢。这个脚本不靠 run：
#   1. 前台起 serve（HTTP API 常驻）；
#   2. 等它就绪；
#   3. GET /api/plugin?location[directory]=… 让这个 location 初始化——插件在这一步加载并连桥。
# 不需要模型请求，也不依赖 TUI 或 run。
#
# 配置：state/opencode-1093015329/opencode.jsonc 里的 plugins.options。
# 日志：systemd 把 serve 的 stdout/stderr 追加到 state/logs/opencode-serve.log；
#       插件自己的轮次日志在 state/logs/opencode-plugin.log（由 logFile 配置）。
set -euo pipefail

export PATH=/run/current-system/sw/bin:/etc/profiles/per-user/nixos/bin:$PATH

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
STATE_DIR=${ACB_OPENCODE_DIR:-$REPO/state/opencode-1093015329}
PORT=${ACB_OPENCODE_PORT:-4097}
PASSWORD=${ACB_OPENCODE_PASSWORD:-acb-opencode-local}
OC=${ACB_OPENCODE_BIN:-/etc/profiles/per-user/nixos/bin/opencode2}

[[ -d $STATE_DIR ]] || { echo "✗ 没有 opencode 状态目录：$STATE_DIR" >&2; exit 1; }
[[ -f $STATE_DIR/env ]] || { echo "✗ 没有 $STATE_DIR/env" >&2; exit 1; }
[[ -x $OC ]] || { echo "✗ 找不到 opencode2：$OC" >&2; exit 1; }

cd "$STATE_DIR"
# 模型网关钥匙（ACB_MODEL_KEY）给 opencode 用；{env:ACB_MODEL_KEY} 在配置解析时展开。
# shellcheck disable=SC1091
. ./env
export OPENCODE_PASSWORD="$PASSWORD"

"$OC" serve --hostname 127.0.0.1 --port "$PORT" --log-level info --print-logs &
serve_pid=$!
trap 'kill "$serve_pid" 2>/dev/null || true' INT TERM

ready=no
for _ in $(seq 1 60); do
  if curl -fsS -o /dev/null -u "opencode:$PASSWORD" "http://127.0.0.1:$PORT/api/info"; then
    ready=yes
    break
  fi
  sleep 0.5
done
if [ "$ready" != yes ]; then
  echo "opencode serve 在 60 秒内没有就绪（端口 $PORT）" >&2
  exit 1
fi

# 让这个 location 初始化：插件在这一步加载并连桥。
# 不需要模型请求；也不依赖 TUI 或 opencode2 run。
curl -fsS -o /dev/null -u "opencode:$PASSWORD" \
  -G --data-urlencode "location[directory]=$STATE_DIR" \
  "http://127.0.0.1:$PORT/api/plugin" || true

wait "$serve_pid"
