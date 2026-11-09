#!/usr/bin/env bash
# acb-status.sh —— 一条命令看清全链路。只读，不改任何状态。
#
# 为什么要有它：这套东西有两种部署形态（见 docs/guide/operations.md），
# 而且「进程在」不等于「活着」（会话沙箱撤了之后 socket 全废）。所以体检必须看
# 端口 + TCP 连接 + 接口回话，不能看 ps。
#
# 用法：deploy/acb-status.sh [--quiet]
# 退出码：0 全部正常；1 有硬故障（端口/连接/接口任一不通）。
set -uo pipefail

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
LOGDIRS=("$REPO/state/logs" "$HOME/.local/state/acb" "$HOME/.local/share/acb/state/logs")
QUIET=0
[[ ${1:-} == --quiet ]] && QUIET=1

bad=0
say() { (( QUIET )) || echo "$@"; }
ok()   { say "  ✓ $*"; }
warn() { say "  ! $*"; }
die()  { say "  ✗ $*"; bad=1; }

port_open() { ss -tln 2>/dev/null | grep -qE "[:.]$1 "; }
estab_count() { ss -tan 2>/dev/null | grep "[:.]$1 " | grep -c ESTAB; }
newest_log() {
  local name=$1 best=
  for d in "${LOGDIRS[@]}"; do
    [[ -f $d/$name ]] || continue
    if [[ -z $best ]] || [[ $d/$name -nt $best ]]; then best=$d/$name; fi
  done
  printf '%s' "$best"
}

say "== 单元 =="
# 形态 A：launcher 托管（本机当前形态，一个单元带全链路）
# 形态 B：四个单元各管一段（deploy/acb-up.sh 装的那种）
if systemctl --user is-active --quiet acb-bridge.service 2>/dev/null; then
  ok "acb-bridge.service active（launcher 托管 dsh/驱动/opencode）"
else
  die "acb-bridge.service 不是 active —— 起：systemctl --user restart acb-bridge"
fi
for unit in dsh-acb.service acb-drivers.service acb-opencode-serve.service; do
  if systemctl --user list-unit-files "$unit" >/dev/null 2>&1; then
    if systemctl --user is-active --quiet "$unit" 2>/dev/null; then
      ok "$unit active"
    else
      die "$unit 不是 active —— 起：systemctl --user restart $unit"
    fi
  fi
done

say "== 端口 =="
declare -A want=([7700]="桥" [3100]="控制台" [3080]="dsh" [3131]="文件中继")
for p in 7700 3100 3080 3131; do
  if port_open "$p"; then ok "$p ${want[$p]}"; else die "$p ${want[$p]} 没在监听"; fi
done
# opencode 只在有 opencode 会话时才该在，缺了只提醒
if port_open 4097; then ok "4097 opencode"; else warn "4097 opencode 没在监听（没有 opencode 会话时可忽略）"; fi

say "== 连接（判活看 ESTAB，不看 ps）=="
n=$(estab_count 7700)
if (( n >= 1 )); then ok "7700 上 $n 条 ESTAB（驱动已接入）"; else die "7700 上 0 条 ESTAB —— 驱动没连上，重启 acb-bridge"; fi
if (( $(estab_count 3080) >= 1 )); then ok "驱动→dsh 已连接"; else warn "驱动→dsh 无 ESTAB（dsh 刚重启时会有 30s～2.5min 冷启动窗口）"; fi

say "== 接口 =="
# /healthz 也要鉴权（401 是常态，不是故障）。令牌走 acbctl，与系统单元同一个来源。
TOKEN=$(acbctl secrets resolve console-token 2>/dev/null | tail -1)
if [[ -z $TOKEN ]]; then
  warn "取不到控制台令牌（acbctl secrets resolve console-token 失败），跳过接口探测"
elif curl -fsS --max-time 5 -H "Authorization: Bearer $TOKEN" http://127.0.0.1:3100/healthz >/dev/null 2>&1; then
  ok "控制台 /healthz 通（带令牌）"
else
  die "控制台 /healthz 不通（令牌有值但请求失败）"
fi
if command -v acbctl >/dev/null 2>&1; then
  say "  · acbctl status：$(acbctl status --summary 2>&1 | head -1)"
fi

say "== 最近错误（各日志最后 3 条命中）=="
for name in bridge.log drivers.log dsh.log; do
  f=$(newest_log "$name")
  if [[ -z $f ]]; then warn "$name 不存在"; continue; fi
  hits=$(grep -aE "content-blocked|dsh\.unreachable|ETIMEDOUT|panic|error:" "$f" 2>/dev/null | tail -3)
  if [[ -z $hits ]]; then ok "$name 无近期错误（$(basename "$f")）"; else
    say "  · $name ($f)"
    printf '%s\n' "$hits" | cut -c1-160 | sed 's/^/      /'
  fi
done

say ""
if (( bad )); then
  say "结论：有硬故障。整链重拉：systemctl --user restart acb-bridge"
  exit 1
fi
say "结论：全链路正常。"
