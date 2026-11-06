#!/usr/bin/env bash
# npm 安装的 dsh 没有 nix profile：必须显式传 ACB 的 preset patch。
# 不传 --patch 时 dsh 不会加载 ACB preset，桥的会话/模型配置不会生效。
#
# 用法：
#   examples/deploy/dsh-npm-patch.sh
#   DSH_BIN=/path/to/npm/bin/dsh DSH_PORT=3080 examples/deploy/dsh-npm-patch.sh
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)

# 示例只导出白名单 ACB_UNIT_* 值；其它变量保持调用方原样或只做局部变量。
export ACB_UNIT_REPO_DIR="${ACB_UNIT_REPO_DIR:-$REPO}"

DSH_MODE=${DSH_MODE:-patch}
DSH_BIN=${DSH_BIN:-$(command -v dsh || true)}
DSH_PORT=${DSH_PORT:-3080}
PATCH=${ACB_DSH_PATCH:-$ACB_UNIT_REPO_DIR/harnesses/dsh/dsh-mcp.patch.yml}

if [[ $DSH_MODE != patch ]]; then
  echo "DSH_MODE must be 'patch'; npm installs have no nix profile" >&2
  exit 2
fi
if [[ -z $DSH_BIN || ! -x $DSH_BIN ]]; then
  echo "dsh binary not found; set DSH_BIN" >&2
  exit 2
fi
if [[ ! -f $PATCH ]]; then
  echo "ACB preset patch not found: $PATCH" >&2
  exit 2
fi

# npm 装法没有 profile，--patch 是唯一入口；这里显式传，不依赖 PATH 里的包装器。
exec "$DSH_BIN" --patch "$PATCH" --no-open --port "$DSH_PORT"
