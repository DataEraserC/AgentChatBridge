#!/usr/bin/env bash
# 验证 acb-launcher 在 systemd 下的三种路径：正常重启 / 回滚换版本 / 连续失败停机。
#
# 只用 systemd-run --user 起临时 unit，不写持久 unit，不改用户默认环境。
# 依赖：cargo、systemd（user bus）、journalctl。
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
launcher="$repo_root/target/debug/acb-launcher"
# 有些 user systemd 看不到 /tmp（PrivateTmp / 命名空间），临时目录放在 $HOME 下。
mkdir -p "$HOME/.cache"
work=$(mktemp -d "$HOME/.cache/acb-launcher-verify.XXXXXX")
units=()

cleanup() {
  for unit in "${units[@]:-}"; do
    systemctl --user stop "$unit" >/dev/null 2>&1 || true
    systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
  done
  rm -rf "$work"
}
trap cleanup EXIT

if ! command -v systemctl >/dev/null || ! systemctl --user show >/dev/null 2>&1; then
  echo "SKIP: user systemd bus not available" >&2
  exit 2
fi

echo "== build =="
cargo build -p acb-launcher

now=$(date +%s)
stamp="$$-$now"

write_target() {
  local path=$1 label=$2 sleep_for=$3 exit_code=$4
  cat >"$path" <<EOF
#!/usr/bin/env bash
echo "[$label] start pid=\$\$ at=\$(date +%s)"
sleep $sleep_for
echo "[$label] exit $exit_code"
exit $exit_code
EOF
  chmod +x "$path"
}

current="$work/current.sh"
lkg_good="$work/lkg-good.sh"
lkg_bad="$work/lkg-bad.sh"
write_target "$current" "v-current" 0.2 1
write_target "$lkg_good" "v-lkg-good" 300 0
write_target "$lkg_bad" "v-lkg-bad" 0.2 1

make_config() {
  local path=$1 lkg=$2 holds=$3
  cat >"$path" <<EOF
[supervisor]
program = "$current"
state_dir = "$work/state"
probe_interval_secs = 1
stable_window_secs = 60

[supervisor.policy]
provider = "acb.supervisor.default"
startup_grace_secs = 0
consecutive_failures = ${holds}
window_failures = 0
unhealthy_secs = 0
unhealthy_probes = 0

[supervisor.lkg]
program = "$lkg"
EOF
}

start_unit() {
  local unit=$1 config=$2
  systemd-run --user --unit="$unit" \
    --property=Type=notify \
    --property=NotifyAccess=main \
    --property=WatchdogSec="${WATCHDOG_SEC:-120}" \
    --property=Restart=on-failure \
    --property=RestartSec=1 \
    --property=RestartForceExitStatus=75 \
    --property=RestartPreventExitStatus=78 \
    --property=StartLimitIntervalSec=60 \
    --property=StartLimitBurst=3 \
    --property=StartLimitAction=none \
    --property=TimeoutStopSec=45 \
    --working-directory="$work" \
    "$launcher" --config "$config"
  units+=("$unit")
}

status() {
  local unit=$1
  echo "-- systemctl status $unit --"
  systemctl --user status "$unit" --no-pager -l || true
}

logs() {
  local unit=$1
  echo "-- journal: $unit --"
  journalctl --user -u "$unit" --no-pager -n 40 || true
}

wait_for_file_value() {
  local file=$1 needle=$2 timeout=${3:-20}
  local elapsed=0
  while ((elapsed < timeout * 10)); do
    if [[ -s $file ]] && grep -q "$needle" "$file"; then
      return 0
    fi
    sleep 0.1
    elapsed=$((elapsed + 1))
  done
  return 1
}

echo
echo "== 场景 0：watchdog 心跳（WatchdogSec=4，launcher 每 2s 发一帧）=="
config_w="$work/w.toml"
make_config "$config_w" "$lkg_good" 99
unit_w="acb-launcher-verify-w-$stamp"
WATCHDOG_SEC=4 start_unit "$unit_w" "$config_w"
sleep 1
pid_w1=$(systemctl --user show -p MainPID --value "$unit_w")
nrestarts_w1=$(systemctl --user show -p NRestarts --value "$unit_w")
sleep 10
pid_w2=$(systemctl --user show -p MainPID --value "$unit_w")
nrestarts_w2=$(systemctl --user show -p NRestarts --value "$unit_w")
echo "WatchdogUSec=$(systemctl --user show -p WatchdogUSec --value "$unit_w")"
echo "NRestarts before=$nrestarts_w1 after=$nrestarts_w2；MainPID before=$pid_w1 after=$pid_w2"
if [[ "$pid_w1" != "$pid_w2" || "$nrestarts_w2" != "0" ]]; then
  echo "FAIL: watchdog 心跳没有生效（unit 被重启）" >&2
  status "$unit_w"
  logs "$unit_w"
  exit 1
fi
status "$unit_w"
systemctl --user stop "$unit_w" >/dev/null 2>&1 || true

echo
echo "== 场景 1：正常重启（supervisor hold，launcher 不死）=="
config_a="$work/a.toml"
make_config "$config_a" "$lkg_good" 99
unit_a="acb-launcher-verify-a-$stamp"
start_unit "$unit_a" "$config_a"
sleep 6
pid1=$(systemctl --user show -p MainPID --value "$unit_a")
sleep 4
pid2=$(systemctl --user show -p MainPID --value "$unit_a")
echo "MainPID before=$pid1 after=$pid2（正常重启应保持不变）"
status "$unit_a"
logs "$unit_a"
systemctl --user stop "$unit_a" >/dev/null 2>&1 || true

echo
echo "== 场景 2：策略 Rollback → 75 → systemd 重拉 → current.json 切 LKG =="
config_b="$work/b.toml"
make_config "$config_b" "$lkg_good" 1
unit_b="acb-launcher-verify-b-$stamp"
start_unit "$unit_b" "$config_b"
if ! wait_for_file_value "$work/state/current.json" "$lkg_good" 20; then
  echo "FAIL: current.json 没有切到 LKG" >&2
  status "$unit_b"
  logs "$unit_b"
  exit 1
fi
sleep 2
status "$unit_b"
logs "$unit_b"
echo "-- current.json --"
cat "$work/state/current.json"
systemctl --user stop "$unit_b" >/dev/null 2>&1 || true

echo
echo "== 场景 3：回滚后的版本也失败 → 78 → failed，不再复活 =="
rm -rf "$work/state" # 清掉场景 2 的指针与 LKG
config_c="$work/c.toml"
make_config "$config_c" "$lkg_bad" 1
unit_c="acb-launcher-verify-c-$stamp"
start_unit "$unit_c" "$config_c"
deadline=$((SECONDS + 25))
while ((SECONDS < deadline)); do
  state=$(systemctl --user is-active "$unit_c" || true)
  if [[ "$state" == "failed" ]]; then
    break
  fi
  sleep 0.2
done
status "$unit_c"
logs "$unit_c"
state=$(systemctl --user is-active "$unit_c" || true)
echo "is-active=$state"
if [[ "$state" != "failed" ]]; then
  echo "FAIL: 连续回滚失败后 unit 没有停在 failed" >&2
  exit 1
fi
pid3=$(systemctl --user show -p MainPID --value "$unit_c")
echo "MainPID after failed=$pid3（应为空/0，证明没有复活）"
if [[ "${pid3:-0}" != "0" ]]; then
  echo "FAIL: unit failed 后仍有 MainPID=$pid3" >&2
  exit 1
fi
echo "PASS: 连续回滚失败停在 failed"
