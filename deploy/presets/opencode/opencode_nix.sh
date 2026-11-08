#!/usr/bin/env bash
# opencode_nix.sh —— opencode 组件的 nix 预设脚本（搬自 deploy/opencode-serve.sh）。
#
# 与旧脚本的差别：
#   1. 不再有明文默认密码：密码必须来自 ACB_ENV_FILE 或显式环境，缺失即失败；
#   2. 不再写死 /etc/profiles/per-user/... 与 PATH，二进制从 ACB_OPENCODE_BIN
#      或 PATH 解析；
#   3. serve 仍是内部后台进程 + 前台等待，所以组件必须 kill_mode = group。
set -euo pipefail

fail() { echo "opencode: $*" >&2; exit 1; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

STATE_DIR=${ACB_OPENCODE_DIR:-}
PORT=${ACB_OPENCODE_PORT:-4097}
HOST=${ACB_OPENCODE_HOST:-127.0.0.1}
PASSWORD=${ACB_OPENCODE_PASSWORD:-}

[[ -n $STATE_DIR ]] || fail "没有 opencode 状态目录（用 ACB_OPENCODE_DIR 指定）"
[[ -d $STATE_DIR ]] || fail "没有 opencode 状态目录：$STATE_DIR"
[[ -n $PASSWORD ]] || fail "没有 opencode 密码（放进 ACB_ENV_FILE 的 ACB_OPENCODE_PASSWORD）"

OC=${ACB_OPENCODE_BIN:-}
if [[ -z $OC ]]; then
  OC=$(command -v opencode2 || true)
fi
[[ -n $OC && -x $OC ]] || fail "找不到 opencode2（用 ACB_OPENCODE_BIN 指定）"

cd "$STATE_DIR"
# opencode 自己的模型网关钥匙等变量；存在才加载，缺失时不伪造。
if [[ -f $STATE_DIR/env ]]; then
  set -a
  # shellcheck disable=SC1091
  . "$STATE_DIR/env"
  set +a
fi
export OPENCODE_PASSWORD="$PASSWORD"

"$OC" serve --hostname "$HOST" --port "$PORT" --log-level info --print-logs &
serve_pid=$!
trap 'kill "$serve_pid" 2>/dev/null || true' INT TERM

ready=no
for _ in $(seq 1 120); do
  if curl -fsS -o /dev/null -u "opencode:$PASSWORD" "http://127.0.0.1:$PORT/api/info"; then
    ready=yes
    break
  fi
  sleep 0.5
done
[[ $ready == yes ]] || fail "serve 在 60 秒内没有就绪（端口 $PORT）"

# 让这个 location 初始化：插件在这一步加载并连桥。
curl -fsS -o /dev/null -u "opencode:$PASSWORD" \
  -G --data-urlencode "location[directory]=$STATE_DIR" \
  "http://127.0.0.1:$PORT/api/plugin" || true

wait "$serve_pid"
