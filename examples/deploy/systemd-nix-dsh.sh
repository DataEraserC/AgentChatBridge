#!/usr/bin/env bash
# 示例：systemd 部署值只从 ACB_UNIT_* 进入 acbctl，unit 内容不落在脚本里。
#
# 用法（当前直接调 acbctl；将来 `acb unit ...` 由现有 acb 委派给 acbctl，参数面不变）：
#   examples/deploy/systemd-nix-dsh.sh                        # 等价 render --print
#   examples/deploy/systemd-nix-dsh.sh render --print
#   examples/deploy/systemd-nix-dsh.sh install --root /tmp/units
#
# 真实值放 deploy/acb.env（不入库）或 ACB_SOURCE_ENV 指向的文件；脚本只把
# 白名单 ACB_UNIT_* 传给 acbctl，其它变量（例如 ACB_CONSOLE_TOKEN）不会进入子进程。
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

# 仓库布局兜底；显式值优先。只写路径/端口/profile/env 文件，不写 unit 内容。
: "${ACB_UNIT_REPO_DIR:=$REPO}"
: "${ACB_UNIT_LOG_DIR:=$REPO/state/logs}"
: "${ACB_UNIT_PROFILE_DIR:=/etc/profiles/per-user/$(id -un)}"
: "${ACB_UNIT_BIN_DIR:=$REPO/result/bin}"
: "${ACB_UNIT_SHELL:=/run/current-system/sw/bin/bash}"
: "${ACB_UNIT_CONFIG_FILE:=$REPO/config.toml}"
: "${ACB_UNIT_ENV_FILE:=$REPO/deploy/acb.env}"
: "${ACB_UNIT_OPENCODE_DIR:=$REPO/state/opencode-1093015329}"
: "${ACB_UNIT_CONSOLE_PORT:=3100}"
: "${ACB_UNIT_OPENCODE_PORT:=4097}"
# 0 = 只安装不 enable；1 = 渲染出 [Install] WantedBy=default.target。
: "${ACB_UNIT_INSTALL_SECTION:=0}"

UNIT_KEYS=(
  ACB_UNIT_REPO_DIR
  ACB_UNIT_LOG_DIR
  ACB_UNIT_PROFILE_DIR
  ACB_UNIT_BIN_DIR
  ACB_UNIT_SHELL
  ACB_UNIT_CONFIG_FILE
  ACB_UNIT_ENV_FILE
  ACB_UNIT_OPENCODE_DIR
  ACB_UNIT_CONSOLE_PORT
  ACB_UNIT_OPENCODE_PORT
  ACB_UNIT_INSTALL_SECTION
)

EXPORTS=()
for key in "${UNIT_KEYS[@]}"; do
  if [[ -z ${!key:-} ]]; then
    echo "missing required variable: $key" >&2
    exit 2
  fi
  EXPORTS+=("$key=${!key}")
done

# 调用方环境里若混入其它 ACB_UNIT_*，提前失败；acbctl 自身也会拒绝。
for key in $(compgen -e); do
  [[ $key == ACB_UNIT_* ]] || continue
  known=0
  for allowed in "${UNIT_KEYS[@]}"; do
    if [[ $key == "$allowed" ]]; then
      known=1
      break
    fi
  done
  if [[ $known -eq 0 ]]; then
    echo "unknown ACB_UNIT_* variable: $key" >&2
    exit 2
  fi
done

# 优先用仓库自己构建的 acbctl；调用面仍是 `unit <args>`，将来由 acb 委派。
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

if [[ $# -eq 0 ]]; then
  set -- render --print
fi

exec env -i "${EXPORTS[@]}" "$ACBCTL_BIN" unit "$@"
