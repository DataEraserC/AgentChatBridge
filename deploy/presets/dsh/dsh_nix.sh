#!/usr/bin/env bash
# dsh_nix.sh —— dsh 组件的 nix 预设脚本（搬自 deploy/components/dsh/start-dsh.sh）。
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

# 走包装器（`dsh` → `dsh-seed-profile` → `dsh-real`）时不传 `--patch`：
# 包装器自己会把 `$DSH_HOME/cordis.patch.yml` 软链好再启动，
# `--patch` 是 `dsh-real` 的选项，包装器不认。
#
# 判据：`ACB_DSH_BIN` 的文件名不是 `dsh-real` 就按包装器处理。
# 不靠「包装器一定叫 dsh」这种猜测——store 路径里的名字是可验证的事实。
USE_WRAPPER=0
if [[ -n ${ACB_DSH_BIN:-} && $(basename "$ACB_DSH_BIN") != "dsh-real" ]]; then
  USE_WRAPPER=1
fi

PROFILE=${ACB_DSH_PROFILE:-nix-web-acb}
PORT=${ACB_DSH_PORT:-3080}

# 启动前检查「ACB 的默认 preset/模型写到了整个 profile」是否已经发生。
#
# 只报告、不静默改写：默认值同时是用户 Settings UI 要写的字段，抢写会让用户
# 刚选的默认值又变回去。修复由 `dsh_defaults_guard.mjs --apply` 显式触发。
# 守卫不在（旧 payload / 未安装）时静默跳过：它不是 dsh 起不来的原因，
# 不该因为缺少它就让组件启动失败。
check_defaults() {
  guard=${ACB_DSH_DEFAULTS_GUARD:-}
  [[ -n $guard && -x $guard ]] || return 0
  home=${ACB_DSH_HOME:-${DSH_HOME:-}}
  [[ -n $home ]] || return 0
  local out status
  out=$("$guard" --dsh-home "$home" --profile "$PROFILE" 2>&1) && status=0 || status=$?
  # 0=干净，3=有漂移；其它（2=用法/环境）也算清过，不挡启动。
  if [[ $status -eq 3 ]]; then
    echo "dsh: 检测到默认值漂移（不挡启动；用 dsh_defaults_guard.mjs --apply 修复）：" >&2
    [[ -n $out ]] && printf '%s\n' "$out" | sed 's/^/dsh:   /' >&2
  fi
  return 0
}
check_defaults

DSH_BIN=${ACB_DSH_BIN:-}
if [[ -z $DSH_BIN ]]; then
  wrapper=$(command -v dsh || true)
  [[ -n $wrapper ]] || fail "PATH 里没有 dsh，没法定位 dsh-real（或用 ACB_DSH_BIN 指定）"
  DSH_BIN="$(dirname "$(readlink -f "$wrapper")")/dsh-real"
fi
[[ -x $DSH_BIN ]] || fail "找不到可执行的 dsh-real：$DSH_BIN"

if [[ $USE_WRAPPER -eq 1 ]]; then
  # 包装器：patch 由它自己从 $DSH_HOME/cordis.patch.yml 读，不传 --patch。
  exec "$DSH_BIN" --profile "$PROFILE" --no-open --port "$PORT"
fi
exec "$DSH_BIN" --profile "$PROFILE" --patch "$PATCH" --no-open --port "$PORT"
