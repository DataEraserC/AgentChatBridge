#!/usr/bin/env bash
# dsh_nix.sh —— dsh 组件的 nix 预设脚本（搬自 deploy/start-dsh.sh）。
#
# 与旧 systemd 入口的差别：
#   1. 不写死 nix store 路径：DSH_BIN 优先取 ACB_DSH_BIN，否则用 PATH 里的
#      dsh 包装器定位同目录的 dsh-real。包装器（dsh-seed-profile）在 profile
#      指纹不匹配时会把 nix 快照整份覆盖到 DSH_HOME，包括 profile 的
#      package.json —— 而 `dsh.profile.bundles` 就在里面，走包装器启动会清掉
#      dsh-presets 登记的动态 bundle。这里只 exec 本体，避免每次重启把这些
#      登记重写掉；
#   2. patch / profile / port 全部来自环境（ACB_DSH_PATCH / ACB_DSH_PROFILE /
#      ACB_DSH_PORT），缺 patch 直接失败，不猜；
#   3. exec 本体，进程组由管理内核按 kill_mode 回收。
set -euo pipefail

fail() { echo "dsh: $*" >&2; exit 1; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

PATCH=${ACB_DSH_PATCH:-}
[[ -n $PATCH ]] || fail "没有 dsh patch（用 ACB_DSH_PATCH 指定）"
[[ -f $PATCH ]] || fail "dsh patch 不存在：$PATCH"

PROFILE=${ACB_DSH_PROFILE:-nix-web-acb}
PORT=${ACB_DSH_PORT:-3080}

DSH_BIN=${ACB_DSH_BIN:-}
if [[ -z $DSH_BIN ]]; then
  wrapper=$(command -v dsh || true)
  [[ -n $wrapper ]] || fail "PATH 里没有 dsh，没法定位 dsh-real（或用 ACB_DSH_BIN 指定）"
  DSH_BIN="$(dirname "$(readlink -f "$wrapper")")/dsh-real"
fi
[[ -x $DSH_BIN ]] || fail "找不到可执行的 dsh-real：$DSH_BIN"

exec "$DSH_BIN" --profile "$PROFILE" --patch "$PATCH" --no-open --port "$PORT"
