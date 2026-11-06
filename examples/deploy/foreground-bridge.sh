#!/usr/bin/env bash
# 前台最小：直接 exec bridge，不经过 launcher/supervisor。
# 明确边界：没有任何回滚、健康循环、75/78 重拉或限流；适合一次手工验证。
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)

export ACB_UNIT_REPO_DIR="${ACB_UNIT_REPO_DIR:-$REPO}"
export ACB_UNIT_BIN_DIR="${ACB_UNIT_BIN_DIR:-$REPO/result/bin}"
export ACB_UNIT_CONFIG_FILE="${ACB_UNIT_CONFIG_FILE:-$REPO/config.toml}"

if [[ ! -x $ACB_UNIT_BIN_DIR/acb-bridge ]]; then
  echo "acb-bridge not executable: $ACB_UNIT_BIN_DIR/acb-bridge" >&2
  exit 2
fi
if [[ ! -f $ACB_UNIT_CONFIG_FILE ]]; then
  echo "config not found: $ACB_UNIT_CONFIG_FILE" >&2
  exit 2
fi

# 没有 launcher，也没有 supervisor：进程退出后没人回滚或重新拉起。
exec "$ACB_UNIT_BIN_DIR/acb-bridge" --config "$ACB_UNIT_CONFIG_FILE"
