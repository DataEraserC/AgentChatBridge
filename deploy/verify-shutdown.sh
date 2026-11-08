#!/usr/bin/env bash
# 停机后复核：launcher 退出后不应留下任何**应随它收敛**的后代。
#
# 口径（区分「保活」与「残留」）：
#   - `type = "oneshot"` 且 `stop_action = "leave_running"` 的组件（当前是
#     snowluma 容器）本来就该在 launcher 退出后继续运行。它们的：
#       * 探活端口（readiness.port）
#       * compose 里发布的宿主机端口（含 .env 覆盖）
#       * podman 容器进程树（容器 init / conmon / rootlessport）
#     一律记为 INFO，不算残留。
#   - 其余端口、进程、scope、journal 记录仍按残留判 FAIL。
#   - 真孤儿（acb-issue-tools / acb-notifier / dsh-subprocess-*，以及不属于
#     leave_running 容器的 remains-running 进程）不会被豁免。
#   - unit cgroup 里的 sidecar（例如 dsh 插件拉起的 mihomo/sing-box）即使已经
#     被 reparent 到 systemd，也仍在 unit cgroup 内：只要它不在 launcher 的
#     进程子树里、又不属于 leave_running 容器，就判 FAIL（第 5 项）。
#
# 用法（把 unit 停掉之后再跑）：
#   deploy/verify-shutdown.sh                       # 默认 acb-bridge.service（现场 launcher unit 名）
#   deploy/verify-shutdown.sh acb-launcher.service
#   SINCE="2026-11-08 20:00:00" deploy/verify-shutdown.sh acb-bridge.service
#   SUPERVISOR_FILE=/path/to/supervisor.toml deploy/verify-shutdown.sh
#
# 退出码：0 全部通过；1 有残留；2 环境缺依赖。
set -uo pipefail

unit=${1:-acb-bridge.service}
since=${SINCE:-5 minutes ago}
ports=(3080 4097 53001 53000 3101 3102 3103 3104 7700)
state_dir=${ACB_STATE_DIR:-$HOME/.local/share/acb/state}
supervisor_file=${SUPERVISOR_FILE:-$state_dir/supervisor/supervisor.toml}
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
info() { printf 'INFO  %s\n' "$*"; }

# 保活组件的端口与容器名。读不到就退化为严格口径（全部按残留处理）。
keep_ports=()
keep_containers=()
load_keepalive() {
  [[ -r $supervisor_file ]] || return 1
  command -v python3 >/dev/null || return 1
  local dump
  dump=$(python3 - "$supervisor_file" <<'PY'
import os
import pathlib
import re
import sys
import tomllib


def expand(text, env):
    def repl(match):
        expression = match.group(1)
        parsed = re.match(r"^([A-Za-z_][A-Za-z0-9_]*)(?::-?([^}]*))?$", expression)
        if not parsed:
            return match.group(0)
        name, default = parsed.group(1), parsed.group(2)
        value = env.get(name)
        if value is None or (value == "" and default is not None):
            return default if default is not None else ""
        return value

    return re.sub(r"\$\{([^}]*)\}", repl, text)


def load_env(compose_path, env):
    env_file = compose_path.with_name(".env")
    if not env_file.is_file():
        return
    try:
        lines = env_file.read_text(errors="ignore").splitlines()
    except OSError:
        return
    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        env[key.strip()] = value.strip().strip('"').strip("'")


def collect_compose(compose_path, ports, containers):
    try:
        text = compose_path.read_text(errors="ignore")
    except OSError:
        return
    env = dict(os.environ)
    load_env(compose_path, env)

    match = re.search(r"^(\s*)container_name:\s*(.+?)\s*$", text, re.M)
    if match:
        name = expand(match.group(2).strip().strip('"').strip("'"), env)
        if name:
            containers.append(name)

    match = re.search(r"^(\s*)ports:\s*$", text, re.M)
    if not match:
        return
    base_indent = len(match.group(1))
    for raw in text[match.end():].splitlines():
        stripped = raw.strip()
        if not stripped:
            continue
        if not stripped.startswith("-"):
            break
        if len(raw) - len(raw.lstrip()) <= base_indent:
            break
        item = expand(stripped[1:].strip().strip('"').strip("'"), env)
        parts = item.split(":")
        if len(parts) >= 2 and parts[-2].strip().isdigit():
            ports.append(int(parts[-2]))


def main():
    try:
        with open(sys.argv[1], "rb") as handle:
            config = tomllib.load(handle)
    except Exception:
        return

    ports, containers = [], []
    for component in config.get("component", []):
        if component.get("type") != "oneshot":
            continue
        if component.get("stop_action", "leave_running") != "leave_running":
            continue
        readiness = component.get("readiness") or {}
        port = readiness.get("port")
        if isinstance(port, int):
            ports.append(port)
        compose = component.get("file")
        if compose:
            collect_compose(pathlib.Path(compose), ports, containers)

    if ports:
        print("PORTS " + " ".join(str(port) for port in sorted(set(ports))))
    if containers:
        print("CONTAINERS " + " ".join(sorted(set(containers))))


main()
PY
  ) || return 1
  [[ -n $dump ]] || return 1
  local line
  while IFS= read -r line; do
    case $line in
      PORTS\ *) read -r -a keep_ports <<< "${line#PORTS }" ;;
      CONTAINERS\ *) read -r -a keep_containers <<< "${line#CONTAINERS }" ;;
    esac
  done <<< "$dump"
  return 0
}

if ! load_keepalive; then
  echo "WARN  读不到 $supervisor_file（或缺少 python3）；保活组件无法识别，按严格口径复核" >&2
fi

echo "== 停机复核：unit=$unit（$scope systemd），journal since=$since =="
if (( ${#keep_ports[@]} )); then
  echo "   保活端口：${keep_ports[*]}；保活容器：${keep_containers[*]:-无}"
fi
state=$("${ctl[@]}" is-active "$unit" 2>/dev/null || true)
if [[ "$state" == "active" ]]; then
  echo "WARN  $unit 仍在 active；请先 stop 再复核" >&2
fi

# 保活容器的 cgroup 标识：从 conmon 的 `-c <容器 id>` 反查。
keep_ids=()
for name in "${keep_containers[@]}"; do
  while IFS= read -r pid; do
    [[ -n $pid ]] || continue
    cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || continue
    [[ $cmd == *conmon* && $cmd == *" -n $name "* ]] || continue
    id=$(sed -n 's/.* -c \([0-9a-f]\{64\}\).*/\1/p' <<< "$cmd")
    [[ -n $id ]] && keep_ids+=("$id")
  done < <(pgrep -x conmon 2>/dev/null || true)
done

is_keep_port() {
  local port
  for port in "${keep_ports[@]}"; do
    [[ $port == "$1" ]] && return 0
  done
  return 1
}

keep_port_listening() {
  local port
  for port in "${keep_ports[@]}"; do
    [[ -n $(ss -lntuH "sport = :$port" 2>/dev/null) ]] && return 0
  done
  return 1
}

# 这个 pid 是否属于 leave_running 组件的容器进程树。
is_keepalive_pid() {
  local pid=$1 cgroup cmd id name
  cgroup=$(cat "/proc/$pid/cgroup" 2>/dev/null) || return 1
  for id in "${keep_ids[@]}"; do
    [[ $cgroup == *"libpod-$id.scope/container"* ]] && return 0
  done
  cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || cmd=""
  for name in "${keep_containers[@]}"; do
    [[ $cmd == *conmon* && $cmd == *" -n $name "* ]] && return 0
  done
  # rootlessport 是 podman 的端口转发器，cmdline 不带容器名；只有在保活端口
  # 仍在监听、且自己就在本 unit cgroup 里时才认定是保活容器的运行时。
  if [[ $cmd == rootlessport* && $cgroup == *"$unit"* ]] && keep_port_listening; then
    return 0
  fi
  return 1
}

echo
echo "-- 1. 端口 --"
port_failed=0
for port in "${ports[@]}"; do
  listeners=$(ss -lntuH "sport = :$port" 2>/dev/null || true)
  if [[ -z $listeners ]]; then
    continue
  fi
  if is_keep_port "$port"; then
    info "端口 $port 仍有监听（leave_running 组件预期保留）"
    continue
  fi
  fail "端口 $port 仍有监听"
  echo "$listeners"
  port_failed=1
done
if ((port_failed == 0)); then
  pass "端口无残留：${ports[*]}（保活端口已豁免：${keep_ports[*]:-无}）"
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
  declare -A seen=()
  orphans=()
  while IFS= read -r line; do
    [[ -n $line ]] || continue
    pid=$(sed -n 's/.*Unit process \([0-9]\+\).*/\1/p' <<< "$line")
    if [[ -n $pid && -n ${seen[$pid]:-} ]]; then
      continue
    fi
    [[ -n $pid ]] && seen[$pid]=1
    if [[ -n $pid && ! -d /proc/$pid ]]; then
      info "journal: PID $pid 已不存在（停机后自行退出）"
      continue
    fi
    if [[ -n $pid ]] && is_keepalive_pid "$pid"; then
      info "journal: PID $pid 属于 leave_running 容器的进程树（预期保留）"
      continue
    fi
    orphans+=("$line")
  done <<< "$hits"
  if (( ${#orphans[@]} > 0 )); then
    fail "journal 仍有非保活组件的孤儿进程"
    printf '%s\n' "${orphans[@]}"
  else
    pass "journal 的 remains-running 记录都归属 leave_running 容器"
  fi
else
  pass "journal 无 'remains running after unit stopped'"
fi

echo
echo "-- 5. unit cgroup 残留 --"
cg_rel=$("${ctl[@]}" show -p ControlGroup --value "$unit" 2>/dev/null || true)
main_pid=$("${ctl[@]}" show -p MainPID --value "$unit" 2>/dev/null || true)
if [[ -z $cg_rel || ! -d "/sys/fs/cgroup$cg_rel" ]]; then
  info "无 unit cgroup（unit 未运行过或已完全回收）"
else
  declare -A ppid_map=()
  while read -r pid parent; do
    [[ -n $pid ]] && ppid_map[$pid]=$parent
  done < <(ps -eo pid=,ppid= 2>/dev/null)

  # 这个 pid 是否仍在 launcher（MainPID）的进程子树里。MainPID 为空/0 时
  # 表示 unit 已停：任何 cgroup 成员都不再属于它。
  in_main_tree() {
    local pid=$1 depth=0
    [[ -n ${main_pid:-} && $main_pid != 0 ]] || return 1
    while ((depth < 64)); do
      pid=${ppid_map[$pid]:-}
      [[ -n $pid && $pid != 0 ]] || return 1
      [[ $pid == "$main_pid" ]] && return 0
      ((depth++))
    done
    return 1
  }

  cg_orphans=()
  while IFS= read -r pid; do
    [[ -n $pid ]] || continue
    [[ $pid == "${main_pid:-}" ]] && continue
    is_keepalive_pid "$pid" && continue
    in_main_tree "$pid" && continue
    cmd=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null) || cmd="?"
    pg=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')
    cg_orphans+=("$pid  pgid=${pg:-?}  ${cmd:-?}")
  done < <(find "/sys/fs/cgroup$cg_rel" -maxdepth 3 -name cgroup.procs \
             -exec cat {} + 2>/dev/null | tr -s ' \n' '\n' | sort -u)

  if (( ${#cg_orphans[@]} > 0 )); then
    fail "unit cgroup 内仍有不属于 launcher 子树的进程（sidecar 残留）"
    printf '  %s\n' "${cg_orphans[@]}"
  else
    pass "unit cgroup 内无无主进程（保活容器已豁免）"
  fi
fi

echo
if ((failed)); then
  echo "RESULT: FAIL —— 停机未收敛，见上面的残留项"
  exit 1
fi
echo "RESULT: PASS —— 停机后无残留"
