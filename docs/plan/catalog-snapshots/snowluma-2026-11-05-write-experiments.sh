#!/usr/bin/env bash
# SnowLuma 写入/破坏性动作实验脚本（默认只打印，不调用平台）。
#
# 安全约束：
# - 必须显式 --execute，且 ACB_TEST_GROUP / ACB_TEST_ACCOUNT 非空才会调用平台。
# - 只能对测试群/测试号执行；脚本会拒绝非白名单里的目标。
# - 每个动作都先读基线、执行一次、再读回；失败立即停止。
# - 本脚本在 2026-11-05 的任务里**没有执行**。
#
# 用法（dry-run）：
#   ACB_TEST_GROUP=123456 ACB_TEST_ACCOUNT=10001 \
#     docs/plan/catalog-snapshots/snowluma-2026-11-05-write-experiments.sh
#
# 用法（真的执行，必须人工确认测试群）：
#   ACB_TEST_GROUP=123456 ACB_TEST_ACCOUNT=10001 \
#     docs/plan/catalog-snapshots/snowluma-2026-11-05-write-experiments.sh --execute
#
# token 来源：ACB_SNOWLUMA_TOKEN 环境变量，或 /run/agenix/acb.env。
set -euo pipefail

BASE="${ACB_SNOWLUMA_URL:-http://127.0.0.1:53000}"
EXECUTE=0
[[ "${1:-}" == "--execute" ]] && EXECUTE=1

if [[ -z "${ACB_TEST_GROUP:-}" || -z "${ACB_TEST_ACCOUNT:-}" ]]; then
  echo "拒绝执行：必须设置 ACB_TEST_GROUP 和 ACB_TEST_ACCOUNT（测试群/测试号）" >&2
  exit 2
fi

if [[ -z "${ACB_SNOWLUMA_TOKEN:-}" && -r /run/agenix/acb.env ]]; then
  # shellcheck disable=SC1091
  set -a; . /run/agenix/acb.env; set +a
fi
if [[ -z "${ACB_SNOWLUMA_TOKEN:-}" ]]; then
  echo "缺少 ACB_SNOWLUMA_TOKEN" >&2
  exit 2
fi

call() {
  local action="$1" params="$2"
  if [[ "$EXECUTE" != 1 ]]; then
    printf 'DRY-RUN %s %s\n' "$action" "$params"
    return 0
  fi
  curl -sS --max-time 20 \
    -H "Authorization: Bearer ${ACB_SNOWLUMA_TOKEN}" \
    -H "Content-Type: application/json" \
    -d "$params" \
    "${BASE}/${action}"
  echo
}

group_params() {
  printf '{"group_id":%s%s}' "$ACB_TEST_GROUP" "${1:-}"
}

echo "== group.add_option =="
echo "会改：测试群入群方式；基线用 get_group_admin_settings"
call get_group_admin_settings "$(group_params ',"no_cache":false')"
call set_group_add_option "$(group_params ',"add_type":3')"
call get_group_admin_settings "$(group_params ',"no_cache":true')"
echo "回滚：按基线把 add_type 写回；再读一次 get_group_admin_settings 对比。"

echo "== group.anonymous =="
echo "会改：测试群匿名开关；没有公开读取接口，需 QQ 客户端群设置截图对比"
call set_group_anonymous "$(group_params ',"enable":true')"
call set_group_anonymous "$(group_params ',"enable":false')"
echo "回滚：enable=false；确认客户端匿名开关回到基线。"

echo "== group.anonymous_ban =="
echo "会改：匿名发言者的禁言状态；必须填一个测试匿名成员"
call set_group_anonymous_ban "$(group_params ",\"user_id\":\"${ACB_TEST_ACCOUNT}\",\"duration\":600")"
call set_group_anonymous_ban "$(group_params ",\"user_id\":\"${ACB_TEST_ACCOUNT}\",\"duration\":0")"
echo "回滚：duration=0；用测试成员发一条匿名消息确认恢复。"

echo "== group.tag_add =="
echo "会改：测试群分类标签；先读 get_group_tags 基线"
call get_group_tags '{"group_class":0}'
call add_group_tag "$(group_params ',"tag_name":"acb-test"')"
call get_group_tags '{"group_class":0}'
echo "回滚：delete_group_tag 删除 acb-test；再读 get_group_tags 确认。"

echo "== group.tag_delete =="
echo "会改：删除测试群分类标签；只删本脚本新增的 acb-test"
call delete_group_tag "$(group_params ',"tag_name":"acb-test"')"
call get_group_tags '{"group_class":0}'
echo "回滚：add_group_tag 加回原标签；再读 get_group_tags 确认。"

echo "== account.model_show_set =="
echo "会改：机器人账号机型展示；平台可能只返回 ok 而不生效"
call _get_model_show '{"model":"acb-test"}'
call _set_model_show '{"model":"acb-test"}'
call _get_model_show '{"model":"acb-test"}'
echo "回滚：写回基线 model；若 _get_model_show 仍返回 mock 值，说明未生效。"

echo
echo "全部步骤是「读基线 → 执行 → 读回 → 回滚 → 再读」。"
echo "DRY-RUN 模式没有调用平台；执行必须人工盯着测试群。"
