#!/usr/bin/env bash
# dsh_presets_acb.sh —— dsh-presets 组件的 acb 预设脚本（oneshot，kind = "exit"）。
#
# 做什么：把 `acbctl sessions` 推导出的 dsh agent preset（每个 dsh 会话一条）
# 打成一个**独立的 dsh bundle**，装进运行中 dsh 的 profile，并登记到该 profile
# `package.json` 的有序 `dsh.profile.bundles`。不碰任何 `cordis.patch.yml`，
# 也不改别人 bundle 的内容。
#
# 为什么是 oneshot + after = ["dsh"]，以及依赖重新就绪为什么要重跑：
#   nix 的 `dsh` 是包装器（dsh-seed-profile → dsh-sync-profiles）。当 profile
#   指纹和 nix 快照不一致时，它会用 store 快照**整份覆盖** profile 的
#   `package.json`、`cordis.patch.yml`、`pnpm-workspace.yaml`。被覆盖的
#   `package.json` 里就包含 `dsh.profile.bundles`——也就是说，只要走包装器
#   启动一次，本脚本登记的 bundle 就会被清掉（`node_modules` 里的目录还在，
#   变成孤儿）。所以：
#     - 运行侧必须直接 exec `dsh-real`（见 dsh_nix.sh 的 DSH_BIN 逻辑）；
#     - 一旦 dsh 真的重启，本脚本必须在 dsh **就绪之后**重放，把 bundle 登记
#       重新写回去。`after = ["dsh"]` 加上管理内核「依赖重新就绪时重跑
#       oneshot」正好对应这个时机。
#
# 为什么用 profile 的 node_modules，而不是写 patch：
#   dsh 解析 bundle 时「安装锚点（dsh 包自带的 nix-support/dsh-bundles.json）
#   优先，其次 profile 目录」，也就是
#   `$DSH_HOME/profiles/<profile>/node_modules/<package>` 会被用于 profile
#   本地包。把包放这里、把包名写进有序 bundles 列表即可，不需要 patch 文件。
#
# 为什么包名带内容摘要：
#   dsh HMR 只在「有序 `dsh.profile.bundles` 列表发生变化」时重新组合；
#   同名包内容变化不会触发重载。所以包名取
#   `acb-presets-live-<patch 内容 sha256 前 16 位>`：会话表/端口一变就是新包名，
#   列表随之变化，HMR 才会重新组合。
#
# 冲突策略（B：冲突即失败）：
#   安装前用 `dsh --dump-config` 扫一遍现有组合。只要现有层（静态
#   `acb-presets` bundle、profile/home patch、其它 bundle）已经注册了同名
#   preset id，就**非零退出、不改任何文件**，并把冲突的 preset id、来源和
#   「从 nix-config 删除静态声明」的提示一次列全。本脚本不覆盖同 id，也不替
#   人删 nix-config。
#
# 注意：这里的 preset id 不是纯标签。`drivers_acb.sh` 与现场的 acb-drivers 包装器
# 都把同一个 id 作为 `ACB_PRESET` 传给 `driver.mjs`，后者再作为 `agentPreset`
# 传给 dsh 的 `session/create`；dsh 按 `config.id` 查表，查不到就报
# Unknown agent preset。改 id 必须两边同步。
#
# 幂等：期望的包名已登记、目录还在、patch 内容一致时直接成功退出，不改 manifest。
#
# 无 HMR（headless/SDK 等组合）时：安装/登记照做，但会明确报「必须重启 dsh」
# 并以非零退出，不假装已生效。重启后本脚本重跑，看到已是期望状态即成功。
#
# 已知限制：只写 node_modules 与 bundles 列表，不写 `dependencies`。如果有人在
# 这个 profile 里跑 `dsh plugin` / pnpm 安装，pnpm 可能裁掉未声明依赖的目录；
# 下次 dsh 重启会触发本脚本重跑，自动补回。
set -euo pipefail

fail() { echo "dsh-presets: $*" >&2; exit 1; }
note() { echo "dsh-presets: $*"; }
warn() { echo "dsh-presets: $*" >&2; }

if [[ -n ${ACB_ENV_FILE:-} && -f ${ACB_ENV_FILE} ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$ACB_ENV_FILE"
  set +a
fi

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# 安装布局：<prefix>/share/acb/presets/dsh-presets/this.sh
PREFIX=$(cd "$HERE/../../../.." && pwd)
ACB_DATA=${ACB_DATA:-${XDG_DATA_HOME:-$HOME/.local/share}/acb}

# 只做 JSON/manifest 改写与摘要，不引入 jq 之类的额外依赖。
NODE=${ACB_NODE_BIN:-}
[[ -n $NODE ]] || NODE=$(command -v node || true)
[[ -x $NODE ]] || fail "找不到 node（用 ACB_NODE_BIN 指定）"

ACBCTL=${ACBCTL_BIN:-}
[[ -n $ACBCTL ]] || ACBCTL=$(command -v acbctl || true)
[[ -n $ACBCTL && -x $ACBCTL ]] || ACBCTL=$PREFIX/bin/acbctl
[[ -x $ACBCTL ]] || fail "找不到 acbctl（用 ACBCTL_BIN 指定）"

CONFIG=${ACB_CONFIG_FILE:-$ACB_DATA/config/config.toml}
[[ -f $CONFIG ]] || fail "找不到 kernel 配置（用 ACB_CONFIG_FILE 指定）：$CONFIG"

DSH_HOME_DIR=${ACB_DSH_HOME:-${DSH_HOME:-${HOME:-}/.dsh}}
[[ -n $DSH_HOME_DIR && $DSH_HOME_DIR != "/.dsh" ]] \
  || fail "没有 DSH_HOME（dsh 未安装/未初始化，安全失败）"
[[ -d $DSH_HOME_DIR ]] || fail "DSH_HOME 不存在：$DSH_HOME_DIR"

PROFILE=${ACB_DSH_PROFILE:-nix-web-acb}
PROFILE_DIR=$DSH_HOME_DIR/profiles/$PROFILE
[[ -d $PROFILE_DIR ]] || fail "dsh profile 不存在：$PROFILE_DIR"
MANIFEST=$PROFILE_DIR/package.json
[[ -f $MANIFEST ]] || fail "profile 没有 package.json：$MANIFEST"
[[ ! -L $MANIFEST ]] || fail "profile 的 package.json 是软链，拒绝写入：$MANIFEST"

NODE_MODULES=$PROFILE_DIR/node_modules
BUNDLE_PREFIX=acb-presets-live-

SESSIONS_OUT=$("$ACBCTL" sessions --config "$CONFIG") || fail "acbctl sessions 失败"

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

# ---- dsh 组合探测 ---------------------------------------------------------
# 冲突检测与 HMR 判断都依赖 `--dump-config`；它只做组合，不启动插件，
# 也不会回写 profile。

DSH_BIN=${ACB_DSH_BIN:-}
PATCH_OVERLAY=${ACB_DSH_PATCH:-}
dump_file=""

resolve_dsh() {
  if [[ -z $DSH_BIN ]]; then
    wrapper=$(command -v dsh || true)
    [[ -n $wrapper ]] || fail "找不到 dsh（用 ACB_DSH_BIN 指定 dsh-real）"
    wrapper=$(readlink -f "$wrapper")
    if [[ -x $(dirname "$wrapper")/dsh-real ]]; then
      DSH_BIN=$(dirname "$wrapper")/dsh-real
    else
      DSH_BIN=$wrapper
    fi
  fi
  [[ -x $DSH_BIN ]] || fail "找不到可执行的 dsh（用 ACB_DSH_BIN 指定）：$DSH_BIN"
  if [[ -n $PATCH_OVERLAY && ! -f $PATCH_OVERLAY ]]; then
    fail "ACB_DSH_PATCH 不存在：$PATCH_OVERLAY"
  fi
}

run_dump() {
  resolve_dsh
  dump_file=$tmp_dir/dump.yml
  if [[ -n $PATCH_OVERLAY ]]; then
    "$DSH_BIN" --profile "$PROFILE" --patch "$PATCH_OVERLAY" --dump-config \
      > "$dump_file" 2> "$tmp_dir/dump.err"
  else
    "$DSH_BIN" --profile "$PROFILE" --dump-config \
      > "$dump_file" 2> "$tmp_dir/dump.err"
  fi || fail "dsh --dump-config 失败（无法安全检查组合）：$(head -c 400 "$tmp_dir/dump.err")"
}

# 判定以 `--dump-config` 输出的**实际组合**为准，而不是看 profile patch 有没有
# 显式声明 hmr：
#   - `@deepseek-ai/dsh-base` 的 patch 默认就带 `- id: hmr`
#     （`disabled: !!js "!ctx.get('profileContext')"`，`config.root: []`）。该表达式
#     的含义是「由 launcher 提供的 profileContext 决定」：base-backed 的 web 组合
#     由 launcher 提供 profileContext，所以运行时是启用的（现场设置页里
#     `include:hmr` 显示「已启用 / 运行中」；`include:<id>` 是 plugin-manager 给
#     这类内置行的 owner-entry 键，dump 里对应的就是 `- id: hmr`）。
#   - headless / SDK / ACP bundle 会在自己的 YAML 里把它整行覆盖成 `disabled: true`。
#   - profile patch 也可以显式 `disabled: false` 再启用。
# 所以：组合里有 hmr 行、且最终没有 `disabled: true` = 启用；整行缺失或最终为
# `disabled: true` 才判为没有 HMR。同一 id 可能被后续层整行重写（后来者覆盖），
# dump 只保留最终一次出现；这里仍按块扫描，只认 `- id: hmr`（不认 `client-hmr`，
# 那是浏览器侧实现，不能当作服务端 HMR）。
hmr_state() {
  awk '
    /^# == / { in_hmr = 0; next }
    /^- id: / {
      in_hmr = 0
      if ($0 == "- id: hmr" || $0 == "- id: include:hmr") {
        seen = 1
        in_hmr = 1
        disabled = 0
      }
      next
    }
    in_hmr && /^  disabled: true$/ { disabled = 1 }
    END {
      if (!seen) print "absent"
      else if (disabled) print "disabled"
      else print "enabled"
    }
  ' "$dump_file"
}

require_hmr() {
  local state
  state=$(hmr_state)
  if [[ $state != enabled ]]; then
    warn "改动已落盘，但该 dsh profile 未启用 HMR（$state）：必须重启 dsh 才会生效。"
    exit 1
  fi
  note "HMR 已启用；bundles 列表变化会触发重新组合，无需重启 dsh"
}

targets=$tmp_dir/targets.tsv
patch_file=$tmp_dir/acb.patch.yml
: > "$targets"
count=0
while IFS=$'\t' read -r sid harness preset port; do
  [[ -n $sid && $harness == dsh ]] || continue
  printf '%s\t%s\t%s\n' "$preset" "$sid" "$port" >> "$targets"
  count=$((count + 1))
done <<< "$SESSIONS_OUT"

# 生成 bundle 的 patch 层。与 nix 静态 bundle 的 preset-row.yml.tmpl 等价：
# 一个 agent preset + 指向本地 MCP 端口的 client + compaction group。
rows_file=$tmp_dir/rows.yml
: > "$rows_file"
while IFS=$'\t' read -r preset sid port; do
  cat >> "$rows_file" <<YAML
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
          # dsh 的 host plane 禁用了 compaction-basic / command-compact /
          # tool-result-pruner，这三个插件只能由 agent preset 挂载；缺这组时
          # ACB 会话既没有 /compact 命令，也不会自动压缩。
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
done < "$targets"

{
  echo "- insert:"
  cat "$rows_file"
} > "$patch_file"

if [[ $count -eq 0 ]]; then
  # 没有 dsh 会话：期望状态是「本脚本的 bundle 不在列表里」。
  # 先看当前有没有，没有就直接成功退出。
  if ! "$NODE" -e '
    const fs = require("fs");
    const data = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const bundles = data?.dsh?.profile?.bundles ?? [];
    process.exit(bundles.some((name) => typeof name === "string" && name.startsWith("acb-presets-live-")) ? 1 : 0);
  ' "$MANIFEST"; then
    if ! "$NODE" - "$MANIFEST" remove <<'NODE' > "$tmp_dir/manifest.json"
const fs = require("fs");
const [manifest] = process.argv.slice(2);
const data = JSON.parse(fs.readFileSync(manifest, "utf8"));
const profile = data && data.dsh && data.dsh.profile;
if (!profile || !Array.isArray(profile.bundles)) {
  console.error("dsh-presets: profile package.json 缺少 dsh.profile.bundles");
  process.exit(3);
}
const before = profile.bundles.slice();
const after = before.filter((name) => !(typeof name === "string" && name.startsWith("acb-presets-live-")));
if (after.length !== before.length) {
  profile.bundles = after;
  const tmp = manifest + ".acb-tmp." + process.pid;
  fs.writeFileSync(tmp, JSON.stringify(data, null, 2) + "\n");
  fs.renameSync(tmp, manifest);
}
process.stdout.write(JSON.stringify({ changed: after.length !== before.length, before, after }) + "\n");
NODE
    then
      fail "写 profile manifest 失败：$MANIFEST"
    fi
    print_bundle_diff() {
      "$NODE" -e '
        const fs = require("fs");
        const r = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
        if (!r.changed) { console.log("  （bundles 列表无变化）"); process.exit(0); }
        const before = new Set(r.before);
        const after = new Set(r.after);
        for (const name of r.before) if (!after.has(name)) console.log("  - " + name);
        for (const name of r.after) if (!before.has(name)) console.log("  + " + name);
      ' "$tmp_dir/manifest.json"
    }
    note "已移除本脚本登记的 bundle（配置里没有 harness = dsh 的会话）："
    print_bundle_diff
    for dir in "$NODE_MODULES/$BUNDLE_PREFIX"*; do
      [[ -e $dir ]] || continue
      rm -rf "$dir"
    done
    run_dump
    require_hmr
    exit 0
  fi
  note "配置里没有 harness = dsh 的会话，也没有本脚本的 bundle；无需改动"
  exit 0
fi

digest=$("$NODE" -e '
  const fs = require("fs");
  const crypto = require("crypto");
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(process.argv[1]));
  process.stdout.write(hash.digest("hex").slice(0, 16));
' "$patch_file") || fail "计算 bundle 摘要失败"
NAME="$BUNDLE_PREFIX$digest"
PKG_DIR=$NODE_MODULES/$NAME

# 幂等判断：manifest 里只有这一个本脚本的包，目录与 patch 内容一致。
# 放在冲突扫描之前，重复执行不再启动 dsh。
"$NODE" -e '
  const fs = require("fs");
  const data = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (!data || !data.dsh || !data.dsh.profile || !Array.isArray(data.dsh.profile.bundles)) {
    console.error("dsh-presets: profile package.json 缺少 dsh.profile.bundles");
    process.exit(3);
  }
' "$MANIFEST" || fail "profile package.json 无效：$MANIFEST"
read_bundles() {
  "$NODE" -e '
    const fs = require("fs");
    const data = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
    const bundles = data?.dsh?.profile?.bundles ?? [];
    process.stdout.write(bundles.join("\n"));
  ' "$MANIFEST"
}
ours=$(
  read_bundles | awk '
    /^acb-presets-live-/ { print }
  '
)
if [[ $ours == "$NAME" && -f $PKG_DIR/presets/acb.patch.yml ]] \
  && cmp -s "$patch_file" "$PKG_DIR/presets/acb.patch.yml"; then
  note "已安装 $NAME（$count 个会话），内容一致；无需改动"
  exit 0
fi

# ---- 冲突检测（安装前，任何人都不改） --------------------------------------
run_dump

# 从组合结果里找同名 preset id。输出层 `# == <name>[, patched by <path>]` 是
# dsh 自己标出的来源；本脚本自己的旧 bundle 层跳过，避免自我冲突。
conflicts=$(awk -F'\t' -v OFS='\t' '
  FNR == NR { want[$1] = $2; next }
  /^# == / {
    meta = $0
    sub(/^# == /, "", meta)
    layer = meta
    origin = meta
    if (meta ~ /, patched by /) {
      split(meta, parts, ", patched by ")
      layer = parts[1]
      origin = parts[2]
    }
    entry = ""
    agent = 0
    next
  }
  layer ~ /^acb-presets-live-/ { next }
  /^- id: / {
    entry = $0
    sub(/^- id: /, "", entry)
    agent = 0
    if (entry ~ /^preset-/) {
      preset = substr(entry, 8)
      if (preset in want && !seen[preset]) {
        seen[preset] = 1
        print preset, want[preset], layer, origin, entry
      }
    }
    next
  }
  /^  name: '"'"'@deepseek-ai\/dsh-agent-preset'"'"'$/ { agent = 1; next }
  /^    id: / {
    if (!agent) next
    cid = $0
    sub(/^    id: /, "", cid)
    if (cid in want && !seen[cid]) {
      seen[cid] = 1
      print cid, want[cid], layer, origin, entry
    }
    next
  }
' "$targets" "$dump_file")

if [[ -n $conflicts ]]; then
  echo "dsh-presets: preset id 冲突，未写入任何文件：" >&2
  while IFS=$'\t' read -r preset sid layer origin entry; do
    if [[ $origin == /* ]]; then
      source="dsh patch $origin"
    elif [[ $layer == acb-presets ]]; then
      source="静态 acb-presets bundle（nix-config 的 modules.dsh.acbSessions）"
    else
      source="bundle $layer"
    fi
    echo "  - $preset（会话 $sid；来源：$source；条目 $entry）" >&2
  done <<< "$conflicts"
  echo "提示：动态 preset id 固定为 acb-<session id>，按策略 B 不与已有定义共存。" >&2
  echo "      请从 nix-config 删除对应会话的静态声明（acbSessions → acb-presets bundle）" >&2
  echo "      后重新启用本组件；本脚本不会覆盖别人的 preset，也不会改 nix-config。" >&2
  exit 1
fi

# ---- 安装包目录 + 原子改 manifest -----------------------------------------

mkdir -p "$NODE_MODULES"
stage=$(mktemp -d "$NODE_MODULES/.$BUNDLE_PREFIX.stage.XXXXXX")
created=0
cleanup_stage() { [[ -d $stage ]] && rm -rf "$stage" || true; }
trap 'cleanup_stage; rm -rf "$tmp_dir"' EXIT

mkdir -p "$stage/presets"
cp "$patch_file" "$stage/presets/acb.patch.yml"
cat > "$stage/package.json" <<JSON
{
  "name": "$NAME",
  "version": "0.1.0",
  "type": "module",
  "dsh": { "bundle": { "patch": "./presets/acb.patch.yml" } }
}
JSON
chmod 644 "$stage/package.json" "$stage/presets/acb.patch.yml"
chmod 755 "$stage" "$stage/presets"

if [[ -d $PKG_DIR ]] && cmp -s "$patch_file" "$PKG_DIR/presets/acb.patch.yml"; then
  rm -rf "$stage"
else
  rm -rf "$PKG_DIR"
  mv "$stage" "$PKG_DIR"
  created=1
fi
cleanup_stage

if ! "$NODE" - "$MANIFEST" "$NAME" install <<'NODE' > "$tmp_dir/manifest.json"
const fs = require("fs");
const [manifest, name] = process.argv.slice(2);
const data = JSON.parse(fs.readFileSync(manifest, "utf8"));
const profile = data && data.dsh && data.dsh.profile;
if (!profile || !Array.isArray(profile.bundles)) {
  console.error("dsh-presets: profile package.json 缺少 dsh.profile.bundles");
  process.exit(3);
}
const ours = (value) => typeof value === "string" && value.startsWith("acb-presets-live-");
const before = profile.bundles.slice();
const after = before.filter((value) => !ours(value));
after.push(name);
if (JSON.stringify(before) !== JSON.stringify(after)) {
  profile.bundles = after;
  const tmp = manifest + ".acb-tmp." + process.pid;
  fs.writeFileSync(tmp, JSON.stringify(data, null, 2) + "\n");
  fs.renameSync(tmp, manifest);
}
process.stdout.write(JSON.stringify({ changed: JSON.stringify(before) !== JSON.stringify(after), before, after }) + "\n");
NODE
then
  [[ $created -eq 1 ]] && rm -rf "$PKG_DIR"
  fail "写 profile manifest 失败：$MANIFEST"
fi

note "已安装 bundle $NAME（$count 个 dsh 会话）："
"$NODE" -e '
  const fs = require("fs");
  const r = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (!r.changed) { console.log("  （bundles 列表无变化）"); process.exit(0); }
  const before = new Set(r.before);
  const after = new Set(r.after);
  for (const name of r.before) if (!after.has(name)) console.log("  - " + name);
  for (const name of r.after) if (!before.has(name)) console.log("  + " + name);
' "$tmp_dir/manifest.json"
note "profile：$PROFILE_DIR"

# manifest 落定后再清理旧包目录（不在列表里的孤儿不影响解析）。
for dir in "$NODE_MODULES/$BUNDLE_PREFIX"*; do
  [[ -e $dir ]] || continue
  [[ $dir == "$PKG_DIR" ]] && continue
  rm -rf "$dir"
done

# ---- HMR 检查 --------------------------------------------------------------
require_hmr
