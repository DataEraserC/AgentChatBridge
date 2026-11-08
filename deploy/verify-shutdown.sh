#!/usr/bin/env bash
# 停机后复核：launcher 退出后不应留下任何后代。
#
# unit 是 KillMode=process，systemd 只杀主进程；本脚本一次性检查四件事，
# 全部为空才算收敛成功：
#   1. 端口 3080 / 4097 / 53001 / 3101-3104 / 7700 没有监听
#   2. 没有 acb-issue-tools / acb-notifier / dsh-subprocess 残留进程
#   3. 没有活动的 dsh-subprocess-*.scope
#   4. 本次停机后 journal 没有 "remains running after unit stopped"
#
# 用法（把 unit 停掉之后再跑）：
#   deploy/verify-shutdown.sh                       # 默认 acb-bridge.service（现场 launcher unit 名）
#   deploy/verify-shutdown.sh acb-launcher.service
#   SINCE="2026-11-08 20:00:00" deploy/verify-shutdown.sh acb-bridge.service
#
# 退出码：0 全部通过；1 有残留；2 环境缺依赖。
set -uo pipefail

unit=${1:-acb-bridge.service}
since=${SINCE:-5 minutes ago}
ports=(3080 4097 53001 3101 3102 3103 3104 7700)
failed=0

if ! command -v ss >/dev/null || ! command -v pgrep >/dev/null; then
  echo "SKIP: 缺少 ss/pgrep" >&2
  exit 2
fi

# 现场的 launcher unit 在 user systemd 下；也兼容 system unit。
scope="system"
ctl=(systemctl)
jctl=(journalctl)
if systemctl --user cat "$unit" >/dev/null 2>&1; then
  scope="user"
  ctl=(systemctl --user)
  jctl=(journalctl --user)
fi

pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; failed=1; }

echo "== 停机复核：unit=$unit（$scope systemd），journal since=$since =="
state=$("${ctl[@]}" is-active "$unit" 2>/dev/null || true)
if [[ "$state" == "active" ]]; then
  echo "WARN  $unit 仍在 active；请先 stop 再复核" >&2
fi

echo
echo "-- 1. 端口 --"
port_failed=0
for port in "${ports[@]}"; do
  listeners=$(ss -lntuH "sport = :$port" 2>/dev/null || true)
  if [[ -n "$listeners" ]]; then
    fail "端口 $port 仍有监听"
    echo "$listeners"
    port_failed=1
  fi
done
if ((port_failed == 0)); then
  pass "端口全空：${ports[*]}"
fi

echo
echo "-- 2. 残留进程 --"
leftovers=$(pgrep -af 'acb-issue-tools|acb-notifier|dsh-subprocess' || true)
if [[ -n "$leftovers" ]]; then
  fail "仍有 acb/dsh 子进程"
  echo "$leftovers"
else
  pass "无 acb-issue-tools / acb-notifier / dsh-subprocess 进程"
fi

echo
echo "-- 3. dsh scope --"
scopes=$("${ctl[@]}" list-units --type=scope --state=active --all --no-legend 2>/dev/null \
  | grep -E 'dsh-subprocess' || true)
if [[ -n "$scopes" ]]; then
  fail "仍有活动的 dsh-subprocess scope"
  echo "$scopes"
else
  pass "无活动 dsh-subprocess scope"
fi

echo
echo "-- 4. journal --"
hits=$("${jctl[@]}" -u "$unit" --since "$since" --no-pager 2>/dev/null \
  | grep -E 'remains running after unit stopped' || true)
if [[ -n "$hits" ]]; then
  fail "journal 仍有孤儿记录"
  echo "$hits"
else
  pass "journal 无 'remains running after unit stopped'"
fi

echo
if ((failed)); then
  echo "RESULT: FAIL —— 停机未收敛，见上面的残留项"
  exit 1
fi
echo "RESULT: PASS —— 停机后无残留"
