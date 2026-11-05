# SnowLuma 写入/破坏性实验记录（2026-11-05）

## 结论（含 2026-11-05 只读调查后的最终判定）

| # | 动作 | 最终判定 | 是否确证生效 | 说明 |
|---|---|---|---|---|
| 1 | `group.add_option` | 成功且已确证（`add_type`）；回滚不完整 | 是（`add_type`） | `2 → 3 → 2` 三次读回一致；关联字段 `member_invite_policy` 留下残留 |
| 2 | `group.anonymous` | 移出实验清单（平台空实现） | 否 | `set_group_anonymous` 的 handler 空实现，不修改任何状态；无读接口 |
| 3 | `group.anonymous_ban` | 移出实验清单（平台空实现） | 否 | handler 空实现，不修改状态；**不需要**制造匿名消息 |
| 4 | `group.tag_add` | 测错对象，移出实验清单 | 否 | 接口操作的是「群分类目录标签」，不是给群打标签 |
| 5 | `group.tag_delete` | 测错对象，移出实验清单 | 否 | 同上；正确参数是 `group_class` + `name`，与测试群无关 |
| 6 | `account.model_show_set` | 移出实验清单（占位实现） | 否 | `_set_model_show` 空实现；`_get_model_show` 只回显 mock |

**残留副作用**：测试群 `1134811973` 的 `member_invite_policy` 由基线 `disabled` 变为
`require_approval`。只读调查确认 SnowLuma 只有一条写入该字段的接口，且平台未应用写入；
**只能人工在 QQ 客户端恢复**，不要继续用接口重试。其余已读回字段与基线一致，标签目录为空。

**第二批候选**（2026-11-05 21:42–21:46）：`group.search` 被平台拒绝（OIDB 1288），
无副作用；`group.member_permissions` 成功且已确证；`member.card` 写入成功但回滚失败，
bot 在测试群的群名片残留 `acb-test-card`。详见「第二批候选」一节。

**脚本未被修改。** 已确认两处脚本问题，见文末。

## 环境与权限

- 端点：`http://127.0.0.1:53000`（实际监听端口，与脚本默认值一致）。
- 运行实例：`app_name=SnowLuma` / `app_version=2.2.1-node` / `protocol_version=v11`。
- token 来源：`/run/agenix/acb.env` 中的 `ACB_SNOWLUMA_TOKEN`；本文不记录 token。
- 测试群：`1134811973`。**未触碰其他群或账号。**
- 权限探测：`get_group_admin_settings` 返回 `retcode=0`；进一步用
  `get_group_member_info` 查询 bot 自身在该群的成员信息，`role=owner`，具备管理权限。
- 基线探测时间：2026-11-05 21:27 +08:00；实验结束时间：2026-11-05 21:32 +08:00。
- 执行方式：没有运行脚本的 `--execute` 全流程；按脚本原样的请求体逐步调用同一实例，
  以便在失败点停下、跳过缺少测试成员的动作。脚本文件未修改。

### Dry-run

实际命令（未调用平台）：

```bash
ACB_TEST_GROUP=1134811973 \
ACB_TEST_ACCOUNT=<bot 自身账号> \
ACB_SNOWLUMA_URL=http://127.0.0.1:53000 \
  docs/plan/catalog-snapshots/snowluma-2026-11-05-write-experiments.sh
```

`ACB_TEST_ACCOUNT` 仅用于 dry-run 打印；真正执行时没有把它当作匿名测试成员使用。

六条计划调用序列（脚本原样）：

```text
get_group_admin_settings {"group_id":1134811973,"no_cache":false}
set_group_add_option    {"group_id":1134811973,"add_type":3}
get_group_admin_settings {"group_id":1134811973,"no_cache":true}
set_group_anonymous     {"group_id":1134811973,"enable":true}
set_group_anonymous     {"group_id":1134811973,"enable":false}
set_group_anonymous_ban {"group_id":1134811973,"user_id":"<ACB_TEST_ACCOUNT>","duration":600}
set_group_anonymous_ban {"group_id":1134811973,"user_id":"<ACB_TEST_ACCOUNT>","duration":0}
get_group_tags          {"group_class":0}
add_group_tag           {"group_id":1134811973,"tag_name":"acb-test"}
get_group_tags          {"group_class":0}
delete_group_tag        {"group_id":1134811973,"tag_name":"acb-test"}
get_group_tags          {"group_class":0}
_get_model_show         {"model":"acb-test"}
_set_model_show         {"model":"acb-test"}
_get_model_show         {"model":"acb-test"}
```

## 逐条记录

### 1. `group.add_option`（写入，已执行）

请求（基线）：

```text
POST /get_group_admin_settings {"group_id":1134811973,"no_cache":false}
```

基线响应：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"disabled","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

执行：

```text
POST /set_group_add_option {"group_id":1134811973,"add_type":3}
```

响应：`{"status":"ok","retcode":0,"data":null}`

执行后读回：

```json
{"status":"ok","retcode":0,"data":{"add_type":3,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

结论：`add_type` 从 `2` 变为 `3`，读回一致，写入生效。**同时发现
`member_invite_policy` 从 `disabled` 变为 `require_approval`。**

回滚：

```text
POST /set_group_add_option {"group_id":1134811973,"add_type":2}
```

响应：`{"status":"ok","retcode":0,"data":null}`

回滚后读回：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

结论：`add_type` 已回到基线 `2`；但 `member_invite_policy` **没有随回滚恢复**。

额外恢复尝试（脚本之外，同一目标群）：

```text
POST /set_group_member_invite_policy {"group_id":1134811973,"policy":"disabled"}
```

响应：

```json
{"status":"failed","retcode":100,"data":null,"wording":"group member invite policy was not applied for group 1134811973"}
```

随后再次读回仍为 `require_approval`。旁证：运行实例的
`GroupAdminApi.getAdminSettings` 每次都直接调用 `FetchGroupDetail`，不使用缓存参数，
因此基线 `disabled` 不是缓存旧值；这是 `set_group_add_option` 在平台侧重置邀请权限位
造成的关联副作用，脚本的回滚只写回 `add_type`，覆盖不到该字段。

### 2. `group.tag_add`（写入，执行失败）

请求（脚本原样）：

```text
POST /add_group_tag {"group_id":1134811973,"tag_name":"acb-test"}
```

响应：

```json
{"status":"failed","retcode":1400,"data":null,"wording":"group_class: is required"}
```

执行前读回：`{"status":"ok","retcode":0,"data":{"tags":[]}}`

执行后读回（分类 `0`）：`{"status":"ok","retcode":0,"data":{"tags":[]}}`

补充只读核查（防止 `group_id` 被当成分类号写入）：分类 `1134811973` 读回
`{"status":"ok","retcode":0,"data":{"tags":[]}}`。

结论：平台在参数校验阶段就拒绝，未产生标签；**这是脚本参数错误**。平台动作
`add_group_tag` 要求 `group_class`（uint32）+ `name`，脚本发送的是 `group_id` + `tag_name`。
后续只读调查进一步确认：该接口操作的是「群分类目录标签」，不是给群打标签；
`group.tag_add` / `group.tag_delete` 属于**测错对象**，应从实验清单移除（见下文
「tag 接口的真实语义」）。

没有改用正确参数重试：`group_class` 是群分类 ID，不是群号，平台也没有「群号 → 分类」
查询接口；用分类参数写标签会作用到该分类下的其他群，超出「只改测试群 1134811973」
的授权边界。

该失败是参数校验拒绝，执行前后读回确认无副作用；因此继续执行下一条独立的群级动作。
`add_option` 出现不可恢复残留后，按 fail-stop 停止后续所有写入。

### 3. `group.tag_delete`（跳过，未执行）

脚本原样的请求应为：

```text
POST /delete_group_tag {"group_id":1134811973,"tag_name":"acb-test"}
```

未执行，原因有两条：

1. 与 `group.tag_add` 同一参数契约问题，`delete_group_tag` 同样要求 `group_class` + `name`。
2. 没有成功创建 `acb-test` 标签，删除无对象；用正确参数执行又会落到分类级，超出授权边界。

### 4. `group.anonymous_ban`（跳过）

脚本要求传入一名测试匿名成员的 `user_id`。当前没有可用的测试匿名成员 user_id；
dry-run 里的 `ACB_TEST_ACCOUNT` 是 bot 自身账号，按任务要求**没有拿 bot 自己试**。
因此跳过，未调用 `set_group_anonymous_ban`。

只读调查确认该 action 的 `run` 是 `() => okResponse()`，**不读取 `user_id`，也不会禁言任何人**。
即使拿到真实匿名发言者的 user_id，也不会有可核对的状态变化；**不需要**去群里制造匿名消息。

### 5. `group.anonymous`（跳过，未执行）

前序 `group.add_option` 的关联字段回滚失败并留下残留，按「任何一步失败立即停止」
停止后续写入，未调用 `set_group_anonymous`。该动作本身也没有公开读接口，即使执行，
开关是否生效也只能靠 QQ 客户端截图确认。

只读调查确认 `set_group_anonymous` 的 `run` 是 `() => okResponse()`，不修改任何状态；
即使补做客户端截图，也不会看到变化。该条已移出实验清单。

### 6. `account.model_show_set`（跳过，未执行）

同样因 fail-stop 未执行。只读调查确认 `_set_model_show` 的 `run` 是
`async () => okResponse()`，`_get_model_show` 只是把请求的 `model` 回显成
`model_show` 并固定 `need_pay: false`；没有可核对的写入语义。

## 只读调查（2026-11-05，未执行写入）

调查对象是运行中的 SnowLuma `2.2.1-node`；只读查看容器 rootfs 的
`/app/runtime/index.mjs`（`package.json:version=2.2.1`，与 `get_version_info` 一致）。

### tag 接口的真实语义

**结论：`get_group_tags` / `add_group_tag` / `delete_group_tag` 操作的是「群分类目录下的
资料标签」，不是给某个群打标签，也不是机器人的分组标签。** `group.tag_add` /
`group.tag_delete` 两条实验属于**测错对象**，从实验清单移除；不要为了跑通把
`group_id` / `tag_name` 硬凑成 `group_class` / `name`。

证据（只读源码）：

- action 定义：`get_group_tags` 的 summary 是「读取某个群分类下的资料标签」，参数只有
  `group_class`（uint32）；`add_group_tag` 是「向某个群分类添加资料标签」，参数
  `group_class` + `name`；`delete_group_tag` 是「从某个群分类移除资料标签」，参数相同。
- 底层协议服务名直接是 `GetGroupCategoryTags` / `AddGroupCategoryTag` /
  `DeleteGroupCategoryTag`，OIDB command `38524` / `38525` / `38526`；请求体只有
  `groupClass`（add/delete 另带 `name`），**没有 `group_id`**。
- `group_class` 只做 uint32 范围校验；平台没有「群号 → 分类」查询接口，无法把写入限定
  到测试群。
- 190-action 快照里没有这三个 tag 动作；运行实例 2.2.1 有。快照与运行实例存在条目差异，
  本轮以运行实例源码为准。

### `member_invite_policy` 的处理结论

**结论：没有第二个接口能改这个字段；`set_group_member_invite_policy` 的写入未被平台应用。
只能人工在 QQ 客户端恢复，不再重试。**

- 当前 `require_approval`，目标 `disabled`；`retcode 100` 是 SnowLuma 的
  `RETCODE.ACTION_FAILED`，不是 OIDB 错误码。堆栈指向
  `GroupAdminApi.setMemberInvitePolicy` 的写后自检：读取最新 `privilegeFlag` 与期望值比较，
  不一致就抛出 `group member invite policy was not applied for group ...`。说明 OIDB 调用
  本身返回了，但平台没改状态。
- **不是权限**：bot 在该群 `role=owner`。
- **不是参数缺失**：action schema 的 `policy` 枚举包含 `disabled`；请求编码包含
  `appPrivilegeFlag`、`appPrivilegeMask=101711872`、`allowMemberInvite=0` 三个字段。
- **不是本地枚举错误**：`disabled` 是合法值。
- 只读证据不能进一步区分「该字段服务端只读」还是「服务端还有额外前置条件」；按可操作结论
  处理：**接口改不了**。
- 没有替代接口：`set_group_add_option` 的请求编码只包含 `addType`（`add_type=4` 时附带
  `groupQuestion` + `groupAnswer`，`add_type=5` 时附带 `groupQuestion`），没有邀请策略或
  `privilegeFlag` 参数；
  `set_group_member_permissions` 只覆盖 `allow_member_upload_album` /
  `allow_member_temporary_session` / `allow_member_create_group`；`set_group_robot_add_option`
  是机器人加群开关。运行实例的 action 目录中只有 `set_group_member_invite_policy` 写邀请权限位。

### 可续跑的最小清单（本轮不执行）

**原六条：0 条可续跑。** `tag_add` / `tag_delete` 测错对象；`anonymous` /
`anonymous_ban` / `model_show_set` 是平台空实现/占位，即使调用成功也只会得到
`okResponse`，不能产生可核对的状态变化。

如果继续扩大写入覆盖，下面三条满足「测试对象可控、有 API 读回、可回滚」。都需要测试群
`1134811973`，不需要其他成员，也不需要制造匿名消息。

| 候选动作 | 测试对象 | 读回方式 | 回滚方式 | 无法确证时 |
|---|---|---|---|---|
| `group.search`（`set_group_search`） | 测试群 | `get_group_admin_settings` 的 `no_finger_open` / `no_code_finger_open` | 写回基线两个字段 | 读回未变 → 标「成功但无法确证（读回未变）」，停止 |
| `group.member_permissions`（`set_group_member_permissions`） | 测试群；bot 是 owner | `get_group_admin_settings` 的 `allow_member_upload_album` / `allow_member_temporary_session` / `allow_member_create_group` | 写回基线三个字段 | 同上 |
| `member.card`（`set_group_card`） | bot 自己在测试群的 `user_id` | `get_group_member_info` 的 `card` | 写回基线 `card`（当前为空） | 同上 |

补充说明：

- 每条仍按「读基线 → 写 → 读回 → 写回基线 → 再读」执行；任何一步读回不符就停止。
- `group.add_option` 的其余 `add_type` 取值（0/1/4/5/55）虽然实现真实、可读回，但每次写
  都会再次重置 `member_invite_policy`，且该字段无法用接口恢复；在人工恢复前不建议续跑。
- `group.anonymous_ban` **不需要**真实匿名 `user_id`：action 的 `run` 是
  `() => okResponse()`，不看 `user_id`，也不会禁言任何人。制造匿名消息没有意义。
- `group.anonymous` 的 `run` 同样是 `() => okResponse()`；`account.model_show_set` 的
  `run` 是 `async () => okResponse()`。这两条没有可核对的写入语义，移出清单。

本轮已执行上表中的三条，结果见下一节「第二批候选」。

## 第二批候选（2026-11-05）

执行协议与第一批一致：读基线 → 执行 → 读回 → 回滚 → 再读。三条逐条状态如下。

| 动作 | 结果 | 是否确证生效 | 说明 |
|---|---|---|---|
| `group.search`（`set_group_search`） | 失败（OIDB 1288） | 否 | 不是只读查询，也没有分页；幂等基线值调用被平台拒绝，读回无变化 |
| `group.member_permissions`（`set_group_member_permissions`） | 成功且已确证 | 是 | `allow_member_upload_album` 由 `true` 改为 `false`，读回一致，再写回 `true` |
| `member.card`（`set_group_card`） | 写入成功且已确证；回滚失败 | 写入是 | 名片由空写为 `acb-test-card`，新鲜读回确认；清空回滚 OIDB 1007，残留 |

### 1. `group.search`（失败，无副作用）

先更正对象：`group.search` 在 SnowLuma 中映射到 `set_group_search`，是**写入动作**，
不是只读查询；该动作也没有分页参数或分页返回，无法验证「分页行为」。本轮没有做真实
开关变更，只用当前基线值 `1/1` 做幂等调用，确认响应结构。

基线（完整管理设置）：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

执行（幂等，写入基线值）：

```text
POST /set_group_search {"group_id":1134811973,"no_finger_open":1,"no_code_finger_open":1}
```

响应：

```json
{"status":"failed","retcode":100,"data":null,"wording":"OIDB error 1288 on 0x89a_0: "}
```

读回：与基线完全一致（`no_finger_open=1`、`no_code_finger_open=1`，其余字段也未变）。

回滚：同一幂等调用，返回相同错误；第二次读回仍与基线一致。

说明：这两次相同调用是在同一个命令序列里发出的，第一次结果未先检查就执行了「回滚」
调用；两次都失败，此后没有再次重试。读回确认没有状态变化，因此没有副作用。

结论：**失败**，平台未应用写入；无法确证开关语义。由于它是写入接口，若后续要确证开关
语义，需要单独授权一次真实变更；本轮按「不产生副作用」的意图没有做真实变更。

### 2. `group.member_permissions`（成功且已确证）

基线（完整管理设置）：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

执行（只改一个权限位）：

```text
POST /set_group_member_permissions {"group_id":1134811973,"allow_member_upload_album":false}
```

响应：`{"status":"ok","retcode":0,"data":null}`

读回：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":false,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

回滚：

```text
POST /set_group_member_permissions {"group_id":1134811973,"allow_member_upload_album":true}
```

响应：`{"status":"ok","retcode":0,"data":null}`

回滚后读回：

```json
{"status":"ok","retcode":0,"data":{"add_type":2,"group_question":"","group_answer":"","robot_member_switch":0,"robot_member_examine":0,"member_invite_policy":"require_approval","allow_member_upload_album":true,"allow_member_temporary_session":true,"allow_member_create_group":true,"new_member_history_visible":false,"no_finger_open":1,"no_code_finger_open":1}}
```

结论：**成功且已确证**。`allow_member_upload_album` 三态读回一致，回滚回到基线；
`member_invite_policy` 仍为 `require_approval`，未被这次写入连带修改。无残留。

### 3. `member.card`（写入成功，回滚失败，有残留）

基线（`get_group_member_info`，默认缓存读）：

```json
{"role":"owner","card":"","card_len":0}
```

执行：

```text
POST /set_group_card {"group_id":1134811973,"user_id":"<bot 自身 user_id>","card":"acb-test-card"}
```

响应：`{"status":"ok","retcode":0,"data":null}`

读回（默认缓存）：`{"role":"owner","card":"","card_len":0}` —— 缓存未刷新，不能作为结论。

新鲜读回（`no_cache:true`）：

```json
{"role":"owner","card":"acb-test-card","card_len":13}
```

回滚（写回基线空字符串）：

```text
POST /set_group_card {"group_id":1134811973,"user_id":"<bot 自身 user_id>","card":""}
```

响应：

```json
{"status":"failed","retcode":100,"data":null,"wording":"OIDB error 1007 on 0x8fc_3: "}
```

回滚后新鲜读回：

```json
{"role":"owner","card":"acb-test-card","card_len":13}
```

结论：**写入成功且已确证；回滚失败，有残留**。平台接受了非空名片写入，但拒绝用空字符串
清空（OIDB 1007）。`set_group_card` 的实现把 `card` 直接作为 `targetName` 发送，没有提供
单独的「清空」参数；按指令没有重试。当前 bot 在测试群的群名片残留为 `acb-test-card`，
需要人工在 QQ 客户端清空。

## 当前状态与人工确认项

- 测试群 `1134811973`：`add_type=2`（与基线一致）；`member_invite_policy=require_approval`
  （基线为 `disabled`，**残留**）；`allow_member_upload_album=true`（已回滚）；
  `no_finger_open=1`、`no_code_finger_open=1`（`group.search` 失败，未变）；
  `allow_member_temporary_session=true`、`allow_member_create_group=true`；其余读回字段
  与基线一致。
- bot 在测试群的群名片：`acb-test-card`（**残留**，基线为空字符串）。
- 群分类标签：分类 `0` 与分类 `1134811973` 均为空，无 `acb-test` 残留。
- 未创建、未修改、未删除任何其他群或账号的数据。
- **需要人工在 QQ 客户端确认/恢复**：
  1. 把测试群「成员邀请方式」恢复为关闭/不允许（对应 `disabled`），并截图留档。
     接口没有第二条可写路径，`set_group_member_invite_policy` 已确认未被平台应用；
     不要再用接口重试。
  2. 把 bot 在测试群的群名片清空（基线为空字符串），并截图留档。
     `set_group_card` 用空字符串清空被平台拒绝（OIDB 1007），不要用接口重试。
  3. `group.anonymous` / `group.anonymous_ban` / `account.model_show_set` 已确认是平台
     空实现/占位，已移出实验清单，不需要再补客户端截图。

## 脚本问题（只报告，未就地修改）

1. **标签实验测错对象**：`add_group_tag` / `delete_group_tag` 操作的是「群分类目录标签」，
   不是给群打标签；脚本按 `group_id` + `tag_name` 调用，平台要求 `group_class` + `name`，
   首次调用即 `retcode 1400`。`group.tag_add` / `group.tag_delete` 应从实验清单移除。
2. **`group.add_option` 回滚不完整**：平台在设置加群方式时会重置成员邀请权限位，
   脚本只把 `add_type` 写回基线，未恢复 `member_invite_policy`；本次实验已实际出现
   该残留，且 `set_group_member_invite_policy` 显式恢复被平台拒绝（`retcode 100`）。
3. `group.anonymous` / `group.anonymous_ban` / `_set_model_show` 在平台侧是未实现或
   占位动作，脚本按「返回 ok」设计，但无法提供可核对的生效证据。
