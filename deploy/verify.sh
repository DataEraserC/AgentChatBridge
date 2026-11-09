#!/usr/bin/env bash
# acb 验证门禁：一条命令跑完所有「机器可判定」的检查。
#
# 设计原则（与 AGENTS.md 对齐）：
#   1. 只跑机器能判定的东西，不做主观评分；
#   2. 全绿才算过；已知的环境相关失败走 deploy/verify-baseline.txt 白名单，
#      白名单之外的任何失败都算红，白名单里已经不复现的条目会单独提示清理；
#   3. 不联网、不写仓库内文件（日志落在 target/verify/，target/ 已被忽略）。
#
# 用法：
#   deploy/verify.sh                  # 全部门禁
#   deploy/verify.sh --quick          # 只跑 fmt + guards（提交前用，秒级）
#   deploy/verify.sh --test-only      # 只跑 cargo test
#   deploy/verify.sh --clippy         # 额外跑 clippy（当前不阻断，仅记警告数）
#   deploy/verify.sh --update-baseline  # 把当前失败写成新的白名单（谨慎！见文件头注释）
#   deploy/verify.sh --help
#
# 环境变量：
#   ACB_VERIFY_NODE=<path>       指定 node（默认取 PATH 里的 node）
#   ACB_VERIFY_BASE=<git ref>     fmt 的对比基线（默认取上游分支，没有则全仓检查）
#   ACB_VERIFY_TIMEOUT=<秒>       单个 cargo test 的超时（默认 2400）
#   ACB_VERIFY_ONLINE=1          允许 cargo 联网（默认 --offline）
#
# 退出码：0 全绿；1 有红；2 用法错误。
set -uo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root" || exit 2

artifact_dir="${ACB_VERIFY_ARTIFACTS:-$repo_root/target/verify}"
mkdir -p "$artifact_dir"

baseline_file="deploy/verify-baseline.txt"
node_bin="${ACB_VERIFY_NODE:-$(command -v node || true)}"
base_ref="${ACB_VERIFY_BASE:-}"
test_timeout="${ACB_VERIFY_TIMEOUT:-2400}"
cargo_net_flag="--offline"
[ "${ACB_VERIFY_ONLINE:-0}" = "1" ] && cargo_net_flag=""

run_fmt=1
run_test=1
run_node=1
run_guards=1
run_clippy=0
update_baseline=0

while [ $# -gt 0 ]; do
  case "$1" in
    --quick) run_test=0; run_node=0 ;;
    --test-only) run_fmt=0; run_node=0; run_guards=0 ;;
    --no-fmt) run_fmt=0 ;;
    --no-node) run_node=0 ;;
    --no-guards) run_guards=0 ;;
    --clippy) run_clippy=1 ;;
    --update-baseline) update_baseline=1 ;;
    -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "未知参数：$1（--help 看用法）" >&2; exit 2 ;;
  esac
  shift
done

if [ -t 1 ]; then
  c_red=$'\033[31m'; c_green=$'\033[32m'; c_yellow=$'\033[33m'; c_dim=$'\033[2m'; c_off=$'\033[0m'
else
  c_red=''; c_green=''; c_yellow=''; c_dim=''; c_off=''
fi

declare -a summary=()
overall=0

record() { # record <阶段> <OK|FAIL|SKIP|WARN> <详情>
  summary+=("$1|$2|$3")
  [ "$2" = "FAIL" ] && overall=1
  local color=$c_green
  [ "$2" = "FAIL" ] && color=$c_red
  [ "$2" = "WARN" ] && color=$c_yellow
  [ "$2" = "SKIP" ] && color=$c_dim
  printf '  %s%-4s%s %s\n' "$color" "$2" "$c_off" "$3"
}

stage_begin() { printf '\n%s== %s ==%s\n' "$c_dim" "$1" "$c_off"; }

# ---------------------------------------------------------------- fmt
# 只检查「本次改动涉及」的 .rs 文件：全仓 fmt 不干净是历史包袱，不该拦住今天的改动，
# 但今天新写的代码必须 fmt 干净。
stage_fmt() {
  stage_begin "fmt（改动文件的 rustfmt）"
  if ! command -v rustfmt >/dev/null 2>&1; then
    record fmt SKIP "没有 rustfmt"
    return
  fi
  if [ -z "$base_ref" ]; then
    base_ref=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)
  fi
  # 基线算不出来就跳过，不判红：「算不出对比范围」是假红，假红会教人忽略门禁。
  # CI 的首次推送 event.before 是一串 0，也走这条路。
  if [ -n "$base_ref" ] && ! git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null 2>&1; then
    record fmt SKIP "基线 ${base_ref} 不可解析（浅克隆或首次推送），跳过格式检查"
    return
  fi
  if [ -z "$base_ref" ]; then
    record fmt SKIP "没有可比较的基线（仓库未设上游分支），跳过格式检查"
    return
  fi
  local -a files=()
  while IFS= read -r f; do [ -n "$f" ] && files+=("$f"); done < <(
    { git diff --name-only --diff-filter=ACMR "$base_ref"...HEAD -- '*.rs' 2>/dev/null
      git diff --name-only --diff-filter=ACMR -- '*.rs' 2>/dev/null
      git ls-files --others --exclude-standard -- '*.rs' 2>/dev/null
    } | sort -u
  )
  if [ ${#files[@]} -eq 0 ]; then
    record fmt OK "相对 $base_ref 没有改动的 .rs 文件"
    return
  fi
  local out="$artifact_dir/fmt.log"
  : >"$out"
  local -a bad=()
  local f
  for f in "${files[@]}"; do
    # 逐个文件跑：rustfmt 一次传多个文件时会在第一个 diff 处就退出，统计不准。
    rustfmt --edition 2024 --check "$f" >>"$out" 2>&1 || bad+=("$f")
  done
  if [ ${#bad[@]} -eq 0 ]; then
    record fmt OK "${#files[@]} 个文件格式正确"
  else
    record fmt FAIL "${#bad[@]}/${#files[@]} 个文件格式不对：${bad[*]}（差异见 $out）"
  fi
}

# ---------------------------------------------------------------- cargo test
# 失败名单 → 扣掉白名单 → 剩下的是红；白名单里没复现的条目提示清理。
stage_test() {
  stage_begin "cargo test（workspace）"
  local log="$artifact_dir/cargo-test.log"
  printf '  %s$ cargo test --workspace --no-fail-fast %s%s\n' "$c_dim" "$cargo_net_flag" "$c_off"
  timeout "$test_timeout" cargo test --workspace --no-fail-fast $cargo_net_flag >"$log" 2>&1
  local rc=$?

  local -a failed=()
  while IFS= read -r name; do
    [ -n "$name" ] && failed+=("$name")
  done < <(grep -oE '^---- .* stdout ----$' "$log" 2>/dev/null | sed -e 's/^---- //' -e 's/ stdout ----$//' | sort -u)

  local passed
  passed=$(grep -aE '^test result:' "$log" | sed -E 's/.* ([0-9]+) passed.*/\1/' | awk '{s+=$1} END {print s+0}')

  if [ $update_baseline -eq 1 ]; then
    {
      printf '# acb verify 白名单：这里列出的失败允许存在，其余一律算红。\n'
      printf '# 每条后面必须写清「为什么可以留」，否则三个月后没人敢删。\n'
      printf '# 新增条目的唯一合法理由是「环境不具备」，不能是「代码暂时不对」。\n'
      for n in "${failed[@]}"; do printf '%s  # TODO 写原因\n' "$n"; done
    } >"$baseline_file"
    record test WARN "已写入 $baseline_file（${#failed[@]} 条），请逐条补原因"
    return
  fi

  local -a allow=()
  if [ -f "$baseline_file" ]; then
    while IFS= read -r line; do
      line="${line%%#*}"; line="$(printf '%s' "$line" | tr -d '[:space:]')"
      [ -n "$line" ] && allow+=("$line")
    done <"$baseline_file"
  fi

  local -a unexpected=() stale=()
  local f a known
  for f in "${failed[@]:-}"; do
    [ -z "$f" ] && continue
    known=0
    for a in "${allow[@]:-}"; do [ "$f" = "$a" ] && known=1 && break; done
    [ $known -eq 0 ] && unexpected+=("$f")
  done
  for a in "${allow[@]:-}"; do
    [ -z "$a" ] && continue
    known=0
    for f in "${failed[@]:-}"; do [ "$f" = "$a" ] && known=1 && break; done
    [ $known -eq 0 ] && stale+=("$a")
  done

  if [ ${#unexpected[@]} -gt 0 ]; then
    record test FAIL "${#unexpected[@]} 条非白名单失败（通过 ${passed:-?}，日志 $log）"
    printf '     %s\n' "${unexpected[@]}"
    return
  fi
  if [ $rc -ne 0 ] && [ ${#failed[@]} -eq 0 ]; then
    record test FAIL "测试没跑起来（编译/链接失败？），日志 $log"
    return
  fi
  local note="通过 ${passed:-?}"
  [ ${#allow[@]} -gt 0 ] && note="$note；白名单内失败 ${#failed[@]} 条"
  record test OK "$note"
  if [ ${#stale[@]} -gt 0 ]; then
    record test-baseline WARN "白名单里这几条已经不复现，可以删：${stale[*]}"
  fi
}

# ---------------------------------------------------------------- harness JS
stage_node() {
  stage_begin "harness JS 测试（node --test）"
  if [ -z "${node_bin:-}" ]; then
    record node SKIP "没有 node"
    return
  fi
  local total_pass=0 total_fail=0 ran=0 dir
  for dir in harnesses/*/test; do
    [ -d "$dir" ] || continue
    local name log out
    name=$(basename "$(dirname "$dir")")
    log="$artifact_dir/node-$name.log"
    if ! ls "$dir"/*.test.mjs >/dev/null 2>&1; then continue; fi
    ( cd "$(dirname "$dir")" && timeout 600 "$node_bin" --test test/*.test.mjs ) >"$log" 2>&1
    out=$(grep -aoE '# (pass|fail) [0-9]+' "$log" | tr -d '#' | tr -s ' ')
    local p fa
    p=$(printf '%s\n' "$out" | awk '/pass/{print $2}')
    fa=$(printf '%s\n' "$out" | awk '/fail/{print $2}')
    total_pass=$((total_pass + ${p:-0}))
    total_fail=$((total_fail + ${fa:-0}))
    ran=$((ran + 1))
    if [ "${fa:-0}" != "0" ]; then
      record node FAIL "harness $name：${fa:-?} 条失败（$log）"
    fi
  done
  if [ $ran -eq 0 ]; then
    record node SKIP "没找到 harness 测试"
    return
  fi
  if [ $total_fail -eq 0 ]; then
    record node OK "$ran 个 harness，$total_pass 条通过"
  fi
}

# ---------------------------------------------------------------- guards
# 便宜、确定、可执行的仓库约束。每条都必须能一句话说清「违反了会怎样」。
stage_guards() {
  stage_begin "guards（仓库约束）"
  local -a problems=() notes=()

  # 1) 规则文件预算：约束写得越长越没人读，超了必须搬走而不是删。
  if [ -f AGENTS.md ]; then
    local lines
    lines=$(wc -l <AGENTS.md)
    if [ "$lines" -le 200 ]; then
      notes+=("AGENTS.md ${lines}/200 行")
    else
      problems+=("AGENTS.md ${lines} 行超过 200 行预算（细则搬到 docs/，这里只留入口）")
    fi
  else
    problems+=("没有 AGENTS.md（新会话的 agent 拿不到约束）")
  fi

  # 2) 运行时状态与本机配置永不入库。
  local leaked=""
  local f
  for f in config.toml deploy/acb.env; do
    if git ls-files --error-unmatch "$f" >/dev/null 2>&1; then leaked="$leaked $f"; fi
  done
  if git ls-files -- 'state/*' | grep -q .; then leaked="$leaked state/"; fi
  if [ -n "$leaked" ]; then
    problems+=("本机文件被 git 跟踪：$leaked")
  else
    notes+=("本机配置/状态未入库")
  fi

  # 3) 密钥形状扫描（只看 git 跟踪的文件）。
  local hits
  hits=$(git ls-files -z | xargs -0 grep -IlnE \
    '(sk-[A-Za-z0-9]{20,}|Bearer [A-Za-z0-9._~+/=-]{24,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|token[[:space:]]*=[[:space:]]*"[A-Za-z0-9._~+/=-]{24,}")' \
    2>/dev/null | head -10)
  if [ -n "$hits" ]; then
    problems+=("疑似密钥落在跟踪文件里：$(printf '%s ' $hits)")
  else
    notes+=("跟踪文件里没有密钥形状的串")
  fi

  local n
  for n in "${notes[@]:-}"; do
    [ -n "$n" ] && printf '  %s·%s %s\n' "$c_dim" "$c_off" "$n"
  done
  if [ ${#problems[@]} -eq 0 ]; then
    record guards OK "${#notes[@]} 条约束通过"
  else
    record guards FAIL "$(printf '%s；' "${problems[@]}")"
  fi
}

# ---------------------------------------------------------------- clippy（可选，不阻断）
stage_clippy() {
  stage_begin "clippy（仅记数，不阻断）"
  local log="$artifact_dir/clippy.log"
  cargo clippy --workspace --all-targets $cargo_net_flag --message-format=short >"$log" 2>&1
  local warns
  warns=$(grep -acE '(^|:) warning:' "$log")
  record clippy WARN "warning 行数 ${warns:-?}（日志 $log）；这个数字是基线，只许降不许升"
}

t0=$SECONDS
printf '%sacb verify%s  %s\n' "$c_dim" "$c_off" "$repo_root"

[ $run_fmt -eq 1 ] && stage_fmt
[ $run_test -eq 1 ] && stage_test
[ $run_node -eq 1 ] && stage_node
[ $run_guards -eq 1 ] && stage_guards
[ $run_clippy -eq 1 ] && stage_clippy

printf '\n%s== 汇总 ==%s（%ds）\n' "$c_dim" "$c_off" $((SECONDS - t0))
for row in "${summary[@]:-}"; do
  [ -z "$row" ] && continue
  stage="${row%%|*}"; rest="${row#*|}"; status="${rest%%|*}"; detail="${rest#*|}"
  color=$c_green; [ "$status" = "FAIL" ] && color=$c_red
  [ "$status" = "WARN" ] && color=$c_yellow; [ "$status" = "SKIP" ] && color=$c_dim
  printf '  %-14s %s%-4s%s %s\n' "$stage" "$color" "$status" "$c_off" "$detail"
done

if [ $overall -eq 0 ]; then
  printf '\n%s全部通过。%s\n' "$c_green" "$c_off"
else
  printf '\n%s有红。修完再提交；确实环境不具备的，按 deploy/verify-baseline.txt 里的规则登记原因。%s\n' "$c_red" "$c_off"
fi
exit $overall
