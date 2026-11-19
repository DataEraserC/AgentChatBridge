#!/usr/bin/env bash
# SnowLuma compose 守卫：盯住「改了会切断 ACB」的几件事。
#
# 为什么需要它：compose 权威文件在 ACB/deploy/，真实端口值却在
# ~/Documents/SnowLuma/.env。podman-compose 只读 compose 同目录的 .env，
# 一旦有人绕过 snowluma.sh 直接手敲 -f，端口会静默退回 3000/3001，
# 而 ACB 按 53000/53001 访问 —— 桥连不上，且没有任何报错提示端口错了。
# 这个脚本把该事故变成可复现的红。
#
# 用法：deploy/verify-snowluma-compose.sh
# 退出码：0 全部通过；1 有约束被违反；2 环境缺依赖（跳过，不算失败）
set -uo pipefail

HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ENV_FILE=${SNOWLUMA_ENV_FILE:-$HOME/Documents/SnowLuma/.env}
failed=0

ok()   { printf '  · %s\n' "$*"; }
bad()  { printf '  ✗ %s\n' "$*"; failed=1; }
skip() { printf 'SKIP: %s\n' "$*" >&2; exit 2; }

command -v python3 >/dev/null 2>&1 || skip "缺少 python3"
[ -f "$ENV_FILE" ] || skip "env 文件不存在：$ENV_FILE（本机未部署 SnowLuma）"

single=$HERE/snowluma-compose.single.yml
multi=$HERE/snowluma-compose.multi.yml
wrapper=$HERE/snowluma.sh

echo "== 文件存在性 =="
for f in "$single" "$multi" "$wrapper"; do
  [ -f "$f" ] && ok "$(basename "$f")" || bad "缺少 $f"
done
[ -x "$wrapper" ] && ok "snowluma.sh 可执行" || bad "snowluma.sh 没有可执行位"

echo "== 包装脚本必须固化 --env-file =="
# 违反后果：从 ACB 目录跑会读不到真实端口，静默切断 ACB。
if grep -q -- '--env-file' "$wrapper"; then
  ok "含 --env-file"
else
  bad "snowluma.sh 未传 --env-file（会退回默认端口切断 ACB）"
fi
# env 缺失必须失败，不能静默继续
if SNOWLUMA_ENV_FILE=/nonexistent-acb-guard "$wrapper" multi config >/dev/null 2>&1; then
  bad "env 文件缺失时仍然继续（应 fail-closed）"
else
  ok "env 缺失时 fail-closed"
fi

echo "== 解析后的端口与卷（加载真实 .env）=="
for mode in single multi; do
  out=$("$wrapper" "$mode" config 2>/dev/null) || { bad "$mode config 解析失败"; continue; }
  MODE=$mode OUT=$out python3 - <<'PY'
import os, sys, yaml
mode = os.environ['MODE']
doc = yaml.safe_load(os.environ['OUT'])
s = doc['services']['snowluma']
ports = [str(p) for p in s['ports']]
host = [p.split(':')[0] for p in ports]
vols = {v.split(':')[1]: v.split(':')[0] for v in s['volumes']}
# 顶层 volumes 的 name: 才是真正挂到哪个具名卷。service 里的引用名不变、
# 只改顶层 name 同样会挂到空卷并丢登录态 —— 必须查解析后的实际 name。
top = doc.get('volumes') or {}
bad = []

# ACB 依赖这两个宿主端口（config.active.toml: 53001=WS, 53000=HTTP）
for p in ('53000', '53001'):
    if p not in host:
        bad.append(f'{mode}: 缺少 ACB 依赖的宿主端口 {p}')
# 登录态三卷改名 = 全部重新扫码
for mount, name in (('/app/data', 'qq-gateway-data'),
                    ('/app/.config', 'qq-client-config'),
                    ('/app/.local/share', 'qq-client-data')):
    ref = vols.get(mount)
    if ref != name:
        bad.append(f'{mode}: {mount} 引用名应为 {name}，实为 {ref}')
    # 引用名对了还不够：顶层 name 决定实际挂哪个卷
    actual = (top.get(ref) or {}).get('name', ref) if ref else None
    if actual != name:
        bad.append(f'{mode}: {mount} 实际卷名为 {actual}（应为 {name}）——会挂到空卷丢登录态')
# 容器名变了会让 podman exec snowluma / supervisor readiness 失效
if s['container_name'] != 'snowluma':
    bad.append(f"{mode}: container_name 应为 snowluma，实为 {s['container_name']}")
if len(set(host)) != len(host):
    bad.append(f'{mode}: 宿主端口重复')

if mode == 'multi':
    import re
    homes = [h.strip() for h in (s['environment'].get('SNOWLUMA_EXTRA_QQ_HOMES') or '').split(',') if h.strip()]
    for h in homes:
        if not h.startswith('/app/'):
            bad.append(f'multi: HOME {h} 不在 /app/ 下（入口脚本会跳过）')
        if not re.fullmatch(r'[A-Za-z0-9_@%+=:,./-]+', h):
            bad.append(f'multi: HOME {h} 含非法字符')
        if h not in vols:
            bad.append(f'multi: HOME {h} 没有对应卷（登录态无处存）')
    if len(set(homes + ['/app'])) != len(homes) + 1:
        bad.append('multi: HOME 之间或与主账号 /app 重叠')

for b in bad:
    print(f'  ✗ {b}')
if not bad:
    print(f'  · {mode}: 端口/卷名/容器名' + ('/HOME 约束' if mode == 'multi' else '') + ' 全部正确')
sys.exit(1 if bad else 0)
PY
  [ $? -ne 0 ] && failed=1
done

echo "== 两份形态不得同时 up（同名容器/同端口）=="
a=$(grep -oE 'container_name:.*' "$single" | head -1)
b=$(grep -oE 'container_name:.*' "$multi" | head -1)
if [ "$a" = "$b" ]; then
  ok "容器名一致（属同一套栈的两种形态，切换需先 down）"
else
  bad "两份 container_name 不一致，切换时会留下孤儿容器"
fi

if [ "$failed" -eq 0 ]; then
  echo "OK: SnowLuma compose 约束全部通过"
else
  echo "FAIL: 见上方 ✗ 条目"
fi
exit "$failed"