#!/usr/bin/env bash
# dsh_presets_acb.sh —— dsh-presets 组件的 acb 预设脚本（oneshot，kind = "exit"）。
#
# 为什么是 oneshot + after = ["dsh"]：
#   dsh 包装器每次启动都会把 nix 算好的 profile 与 home patch 同步到
#   `$DSH_HOME/cordis.patch.yml`（整份覆盖）。所以「按会话生成 agent preset」
#   必须在 dsh **就绪之后**重放一遍，否则会被下一次启动覆盖掉。管理内核在
#   依赖重新就绪时会重跑本脚本，正好对应这件事。
#
# 写哪里：`$DSH_HOME/cordis.patch.yml` 是 dsh 的 HMR 监听目标之一，且不是
#   managed profile 指纹覆盖的文件；写完不需要重启 dsh。
#
# 幂等：文件里只有一段 `# BEGIN/END acb-generated dsh-presets` 托管块，
#   每次整段重写（先删旧段，再追加新段），其余内容原样保留。
# 冲突：同一 `preset-<name>` 已经出现在托管块之外、`ACB_DSH_PATCH` 或
#   `ACB_PRESET_CONFLICT_FILES` 里时报错退出（非零），不覆盖别人的定义。
set -euo pipefail

fail() { echo "dsh-presets: $*" >&2; exit 1; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)

ACBCTL=${ACBCTL_BIN:-}
[[ -n $ACBCTL ]] || ACBCTL=$(command -v acbctl || true)
[[ -x $ACBCTL ]] || ACBCTL=$REPO/result/bin/acbctl
[[ -x $ACBCTL ]] || fail "找不到 acbctl（用 ACBCTL_BIN 指定）"

CONFIG=${ACB_CONFIG_FILE:-$REPO/config.toml}
[[ -f $CONFIG ]] || fail "找不到 kernel 配置（用 ACB_CONFIG_FILE 指定）：$CONFIG"

DSH_HOME_DIR=${ACB_DSH_HOME:-${DSH_HOME:-${HOME:-}/.dsh}}
[[ -n $DSH_HOME_DIR && $DSH_HOME_DIR != "/.dsh" ]] \
  || fail "没有 DSH_HOME（dsh 未安装/未初始化，安全失败）"
[[ -d $DSH_HOME_DIR ]] || fail "DSH_HOME 不存在：$DSH_HOME_DIR"

TARGET=$DSH_HOME_DIR/cordis.patch.yml
[[ -f $TARGET ]] || fail "没有 dsh home patch：$TARGET（dsh 还没初始化？）"

SESSIONS_OUT=$("$ACBCTL" sessions --config "$CONFIG") || fail "acbctl sessions 失败"

BEGIN_MARK='# BEGIN acb-generated dsh-presets'
END_MARK='# END acb-generated dsh-presets'

# 去掉托管块后的正文（用于冲突扫描与重写）。
strip_block() {
  sed "/^${BEGIN_MARK//\//\\/}\$/,/^${END_MARK//\//\\/}\$/d" "$1"
}

# 正文里是否已经出现 `preset-<name>`（按 token 精确比较，避免前缀误报）。
has_preset() {
  awk -v want="preset-$2" '
    { for (i = 1; i <= NF; i++) if ($i == want) { found = 1; exit } }
    END { exit found ? 0 : 1 }
  ' <<< "$1"
}

check_conflicts() {
  local file=$1 label=$2 text
  [[ -f $file ]] || return 0
  if [[ $file == "$TARGET" ]]; then
    text=$(strip_block "$file")
  else
    text=$(cat "$file")
  fi
  while IFS=$'\t' read -r sid harness preset port; do
    [[ -n $sid && $harness == dsh ]] || continue
    if has_preset "$text" "$preset"; then
      fail "preset 冲突：$file 已注册 preset-$preset（$label）"
    fi
  done <<< "$SESSIONS_OUT"
}

check_conflicts "$TARGET" "home patch 托管块之外"
if [[ -n ${ACB_DSH_PATCH:-} ]]; then
  check_conflicts "$ACB_DSH_PATCH" "ACB_DSH_PATCH"
fi
if [[ -n ${ACB_PRESET_CONFLICT_FILES:-} ]]; then
  read -r -a conflict_files <<< "${ACB_PRESET_CONFLICT_FILES//:/ }"
  for file in "${conflict_files[@]}"; do
    check_conflicts "$file" "ACB_PRESET_CONFLICT_FILES"
  done
fi

# 生成托管块。只有 dsh harness 的会话需要 agent preset 与 MCP client。
rows=0
block=$(mktemp)
trap 'rm -f "$block"' EXIT
{
  echo "$BEGIN_MARK"
  echo "- insert:"
  while IFS=$'\t' read -r sid harness preset port; do
    [[ -n $sid && $harness == dsh ]] || continue
    rows=$((rows + 1))
    cat <<YAML
    - id: preset-$preset
      name: '@deepseek-ai/dsh-agent-preset'
      config:
        id: $preset
        plugins:
          - id: mcp-$preset
            name: '@deepseek-ai/dsh-mcp-client'
            config:
              transport: streamable-http
              serverName: acb
              url: http://127.0.0.1:$port/mcp
          - id: compaction
            name: cordis:group
            group: true
            isolate:
              compaction: true
              toolResultPruner: true
            config:
              - id: compaction-basic
                name: '@deepseek-ai/dsh-compaction-basic'
              - id: command-compact
                name: '@deepseek-ai/dsh-command-compact'
              - id: tool-result-pruner
                name: '@deepseek-ai/dsh-compaction-tool-result-pruner'
YAML
  done <<< "$SESSIONS_OUT"
  echo "$END_MARK"
} > "$block"

combined=$(mktemp)
trap 'rm -f "$block" "$combined"' EXIT
strip_block "$TARGET" > "$combined"
if [[ $rows -gt 0 ]]; then
  cat "$block" >> "$combined"
fi

# 同目录 rename，原子替换。
target_tmp="$TARGET.tmp.$$"
cp "$combined" "$target_tmp"
mv -f "$target_tmp" "$TARGET"

echo "dsh-presets: wrote $rows preset row(s) to $TARGET"
