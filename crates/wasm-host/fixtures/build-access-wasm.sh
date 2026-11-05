#!/usr/bin/env bash
# 重新构建参考 wasm 模块。
#
# 需要：rustc + wasm32-unknown-unknown 标准库 + wasm-ld（LLVM lld）。
# 仓库里已提交 access.wasm；只有在改 access_module.rs 后才需要跑这个脚本。
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
rustc \
  --target wasm32-unknown-unknown \
  --crate-type=cdylib \
  --edition 2021 \
  -C linker=wasm-ld \
  -O \
  "$here/access_module.rs" \
  -o "$here/access.wasm"
