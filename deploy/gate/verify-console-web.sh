#!/usr/bin/env bash
# 前端产物守卫：`crates/bridge/src/console_web/` 必须是 `web/` 的最新构建结果。
#
# 为什么需要它：产物提交进仓库，换来的是 `nix build` 保持纯 Rust + `--offline`。
# 代价是「改了 `web/src/` 却忘了重建」，于是二进制里跑的是旧界面，而
# `git diff` 里看不出任何异常——这类漂移只有真机点一遍才会发现，太晚。
#
# 做法：在临时目录里用现有 `web/node_modules` 重建一份，逐文件比对字节。
# 不比对尺寸或时间戳：那会被格式化/压缩器版本差异放过。
#
# 依赖：node + web/node_modules。缺任何一个都按 SKIP 处理并说明原因，
# 不算红——`deploy/verify.sh` 已经把「环境不具备」与「代码不对」分开对待。
set -uo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root" || exit 2

web_dir=web
out_dir=crates/bridge/src/console_web
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fail() { echo "console-web: $*" >&2; exit 1; }
skip() { echo "console-web: SKIP $*"; exit 0; }

[[ -d $web_dir ]] || fail "缺少 $web_dir（前端源码）"
[[ -d $web_dir/node_modules ]] || skip "没有 $web_dir/node_modules；先在该目录 npm install"
command -v node >/dev/null 2>&1 || skip "PATH 里没有 node"

# 图标必须走 `src/icons.ts` 的路径常量，不能写 `mdi-xxx` 字符串。
#
# `vuetify.ts` 用的是 `mdi-svg` 图标集，它不吃字符串名：`<v-icon icon="mdi-arrow-up">`
# 会把 `mdi-arrow-up` 原样写进 `<path d="mdi-arrow-up">`，浏览器画不出任何东西。
# 症状最难查的那种——按钮存在、可点、量得到 20×20、aria-label 也对，只是空白。
# 2026-11-17 真机踩中：用户报「看不到删除和调顺序按钮」。
if grep -rn 'icon="mdi-' "$web_dir/src" --include='*.vue' >/dev/null 2>&1; then
  hits=$(grep -rn 'icon="mdi-' "$web_dir/src" --include='*.vue' | head -5)
  fail "图标写成了字符串名（mdi-svg 画不出来，改用 src/icons.ts 的常量）：
$hits"
fi

# 产物必须存在：仓库里少了它就是漏提交。
for f in index.html app.js app.css; do
  [[ -f $out_dir/$f ]] || fail "缺少产物 $out_dir/$f（构建后忘记提交？）"
done

# 在临时输出目录里重建，避免覆盖仓库里的产物（比对失败时还要用它给出差异）。
build_log=$tmp/build.log
(
  cd "$web_dir" || exit 1
  # 用本地安装的 vite，不走 npx（npx 会联网解析）。
  ./node_modules/.bin/vite build --outDir "$tmp/dist" --emptyOutDir >"$build_log" 2>&1
) || {
  echo "--- vite build 输出 ---" >&2
  tail -30 "$build_log" >&2
  fail "vite build 失败"
}

status=0
for f in index.html app.js app.css; do
  if ! cmp -s "$tmp/dist/$f" "$out_dir/$f"; then
    echo "console-web: $out_dir/$f 与源码构建结果不一致" >&2
    # 给出尺寸差，不打印内容（压缩产物打出来也读不了）。
    built=$(wc -c <"$tmp/dist/$f")
    shipped=$(wc -c <"$out_dir/$f")
    echo "  构建出 ${built} 字节，仓库里 ${shipped} 字节" >&2
    status=1
  fi
done

if [[ $status -ne 0 ]]; then
  echo "修法：cd web && npm run build，然后把 $out_dir/ 的改动一起提交。" >&2
  exit 1
fi

echo "console-web: 产物与 web/ 源码一致（index.html / app.js / app.css）"
