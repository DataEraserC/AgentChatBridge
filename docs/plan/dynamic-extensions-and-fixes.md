# 动态扩展与反馈修复：执行清单（2026-11-03）

来源：ADR-0029（动态扩展）、ADR-0030（unload-safe native）、`/api/feedback` 的 6 条
agent 反馈，以及 2026-11-03 用户新增要求（PluginManager 嵌套样例、unload-safe native）。

执行规则：

1. 按顺序执行，前一项验收通过后再开始下一项。
2. 每项独立提交；提交信息说明依据的 ADR / feedback id。
3. 每项完成后把对应 checkbox 改为 `[x]`，并在该任务下补一行实测结果。
4. 不确定的行为先复现、记录证据，再改代码；不要用“应该”代替实测。

## 0. 问题分类

| 来源 | 内容 | 归类 | 处理位置 |
|------|------|------|----------|
| feedback 1 | 长工具调用后 `send_message` 被拒：no turn is in progress | 轮次票据问题 | ADR-0014 / 0019；本清单 T3 |
| feedback 2/3 | `set_note` term/member 与 status 校验冲突（两条重复） | 工具契约 bug | 直接修；本清单 T1 |
| feedback 4 | 反馈缺少查询、回复、状态和回复后通知 | 反馈生命周期 | ADR-0021；本清单 T7/T8 |
| feedback 5 | memory worktree 实际是独立仓库，不是 linked worktree | 实现 bug | ADR-0017；本清单 T2 |
| feedback 6 | memory 是否支持跨机器远端 Git | 设计问题 | ADR-0017 扩展或新 ADR；本清单 T9 |
| 用户要求 1 | PluginManager 嵌套没有样例 | 示例/文档 | ADR-0026；本清单 T5 |
| 用户要求 2 | 做一个 unload-safe native plugin | 新 ABI | ADR-0030；本清单 T4 |
| 用户要求 3 | 动态扩展、热重载、任务结果、扩展工厂 | 新架构 | ADR-0029；本清单 T6/T7/T10 |

不并入 ADR-0029 的：feedback 1、2/3、5 是具体 bug 或既有 ADR 的实现问题；feedback 6
是 memory 拓扑设计。ADR-0029 只保留与其 D5（任务结果）和 D9（源码拓扑）相关的引用。

## 1. 任务

### T1. 修复 `set_note` 的 status 契约（feedback 2/3）

- [x] 复现：`kind=term` + 不传 status、`kind=term` + `status=""`、`kind=member`、`kind=topic` + open/closed。
- [x] 修正校验：term/member 不接受 topic 状态，但 `status` 缺失或空串不得报错；topic 只接受 open/closed。
- [x] 确认 MCP 工具 schema 中 `status` 不是必填，并更新描述（[tools.mjs](../../harnesses/shared/tools.mjs)）。
- [x] 单元测试覆盖四种组合，错误信息明确。
- 改动：`crates/kernel/src/engine.rs`（`write_note`）、`harnesses/shared/tools.mjs`。
- 验收：term/member 可正常写入；topic 状态校验仍严格；重复反馈 2/3 可由同一修复关闭。
- 实测：新增 `set_note_term_and_member_accept_absent_or_empty_status`；`cargo test -p acb-kernel --test turn_flow` 68/68 通过。

### T2. 修复 memory worktree 的 linked worktree 语义（feedback 5）

- [x] 复现并确认：空裸仓库时 `ensure_worktree` 走 `git init`，产生独立仓库（当前实例的 `.git` 是目录、无 `commondir`）。
- [x] 决定启动顺序：先在主仓库创建初始提交，再创建 linked worktree；或为“空仓库”提供受控的初始化流程。
- [x] 修正 `memory_git_push`：目标 refspec 与实际分支 `acb/<session>` 一致，并明确 `origin` 缺失时的错误。
- [x] 迁移处理：检测已存在的独立 worktree，不删除；空独立仓库改名为 `*.orphan-*` 后重建，非空仓库报错保留。
- [x] 测试：`.git` 是文件、`commondir` 指向主仓库、分支为 `acb/<session>`、add/commit/push 语义正确。
- 改动：`crates/kernel/src/memory.rs`、memory 相关测试。
- 验收：新建会话的 worktree 是主仓库的 linked worktree；提交能回到主仓库；不再出现 `no origin` 伪装成“未配远端”。
- 实测：空裸仓库先种空提交；linked worktree 分支 `acb/<session>`；空独立仓库自动改名为 `*.orphan-*` 后重建，非空独立仓库报错保留；`git_push` 推当前分支。`cargo test --workspace` 全通过，memory 单测 11/11。

### T3. 复现并处理轮次结束后的 `send_message` 拒绝（feedback 1）

- [x] 在桥 API 层复现：`end_turn` 后 `send_message` 被 `no_active_round` 拒绝；真机 dsh-group2 的长工具链没有再次触发。
- [x] 记录代码路径：`turn_ctx` 在 `ended` 分支只放行 `ack_messages` / `report_feedback`，`send_message` 被拒。
- [x] 选择桥侧宽限：轮次结束后 60 秒内允许补发当前房间的 `send_message`；不把“轮次结束后随意发送”变成默认能力，`send_message_to` 仍拒绝。
- [x] 测试覆盖：宽限内补发成功并落到出站通道；其它工具仍按 `no_active_round` 拒绝；stale ticket 测试改用 `send_message_to`。
- 改动：`crates/kernel/src/engine.rs`（`SEND_GRACE`、`turn_ctx`、`no_active_round`）、`crates/kernel/tests/turn_flow.rs`。
- 验收：复现用例不再丢消息；不会把“轮次结束后随意发送”变成默认能力。
- 实测：`cargo test -p acb-kernel --test turn_flow` 69/69；`cargo test --workspace` 全通过。超窗路径由 `SEND_GRACE` 条件保证，未做 60 秒真实等待测试。
- 调试留痕：`end_turn` 记录 `turn_end received`（会话/房间/票/错误）；任何迟到工具记录 `late_tool`（工具、票、距 turn_end 的毫秒数）。dsh driver 同时记录 `tool_call_start` / `tool_call_end` / `turn_end_sent`（ISO 时间），用于判断工具是收轮前已发起、还是收轮后才发起。
- 边界：这是一段桥侧容忍窗口，不是“harness 可以在 turn_end 后继续干活”的契约。真机根因尚未复现；若长工具链仍跨过 60 秒，下一步必须修 harness 的收轮屏障（工具调用全部结算后再发 `turn_end`）。

### T4. 实现 unload-safe native plugin（ADR-0030）

- [ ] 在 `crates/adapter-api` 增加 ABI v2：`acb_plugin_entry_v2`、`AcbPluginV2`、
      `struct_size`、`flags`、`UNLOAD_SAFE`。
- [ ] `acb-adapter-dynamic` 支持 v2：先找 v2 符号，回退 v1；只对 `UNLOAD_SAFE` 走卸载。
- [ ] Root / `PluginLoader` 增加安全点卸载：停止新调用 → 轮次边界 → destroy → 丢弃实例 → dlclose。
- [ ] 第一版只覆盖策略扩展点（wake/access/delivery/send/turn_error/clock/receive/route）；
      平台适配器后置。
- [ ] 增加最小 C 插件 fixture：加载 → invoke → destroy → dlclose → 重新加载。
- [ ] 非 `UNLOAD_SAFE` 插件拒绝卸载并明确报错；v1 行为保持不变。
- 改动：`crates/adapter-api`、`crates/adapters/dynamic`、`crates/kernel/src/plugin.rs`、
  `examples/dynamic/unload-safe-c/`（或等价 fixture）、`docs/08-plugin-development.md`。
- 验收：C fixture 能完成一次完整卸载/重载；桥不崩溃；v1 `.so` 仍能加载。

### T5. 增加 PluginManager 嵌套样例

- [ ] 新增可构建样例（建议 `examples/dynamic/nested-manager/` 或独立 fixture crate），
      父 manager 通过 `children()` 返回子 manager。
- [ ] 子 manager 注册一个可解析的 provider（如 `child.always`），父 manager 不直接提供。
- [ ] 演示 health 顺序、深度限制、环路拒绝中的至少一项。
- [ ] 在 `docs/08-plugin-development.md` 增加“嵌套 manager”小节，指向样例。
- 依据：当前只有 [plugin.rs](../../crates/kernel/src/plugin.rs) 的单元测试
  `nested_manager_subtree_is_installed_and_resolved`，没有面向用户的样例。
- 验收：样例可独立构建；配置引用 `parent.child.<name>` 能解析；文档可从零照抄。

### T6. ADR-0029 P1：动态扩展闭环

- [ ] 增加 `ACB_DYNAMIC_DIR` 与动态模块 manifest（meta/extension/capabilities/scope）。
- [ ] 增加稳定分派器（route 先做），新增/删除模块不改 `config.toml`。
- [ ] 接入真实热重载：解析目录 → 校验/编译 → 轮次边界整体替换 dynamic namespace；
      provider 集合变化也允许。
- [ ] 增加 authoring 工具：`extensions_list` / `extensions_read` / `extensions_write` /
      `extensions_reload`，仅管理员会话，路径 jail、大小上限。
- [ ] 模块级可观察性：summary、effects、source、hash、mtime、health、引用关系、失败计数。
- [ ] 测试：新增模块在下一轮生效；删模块失效；坏模块不替换线上版本；并发写不破坏目录。
- 依据：ADR-0029 P1、D3、D4、D7、D8、D9。
- 验收：AI 写一个 route 模块 → reload → 下一轮真实生效，全程不重启桥。

### T7. 任务结果与反馈回复的唤醒闭环

- [ ] task 注册表与任务记录：task_id、origin、handler、状态、结果、错误、时间。
- [ ] `task_list` / `task_get` 工具；任务完成后写 `kind=task` 合成事件。
- [ ] `wake_on = never|error|always` 快捷条件；默认走现有 wake 策略。
- [ ] 高信任模块的直接唤醒 API，带审计与频率限制；合成事件跳过会再次触发同一 route 的路径。
- [ ] feedback reply 复用同一套事件，回复时可唤醒原会话。
- 依据：ADR-0029 D5、D9.4；feedback 4。
- 验收：任务失败/成功可按策略唤醒；AI 未唤醒也能用 `task_get` 查到结果；无自触发循环。

### T8. 反馈生命周期（feedback 4）

- [ ] `feedback_list` / `feedback_get` / `feedback_reply` 工具；状态 open / in_progress / resolved / wontfix。
- [ ] 控制台 `/api/feedback` 增加回复、状态流转和审计。
- [ ] 维护方回复时按 T7 的事件模型通知原会话。
- [ ] 文档与测试：提交 → 查询 → 回复 → 唤醒/不唤醒两条路径。
- 依据：ADR-0021、ADR-0029 D5。
- 验收：反馈不再只是只投不回的意见箱；回复可被原会话看到。

### T9. Memory 远端与多机拓扑（feedback 6）

- [ ] 先写 ADR（可扩 ADR-0017 或新 ADR-0031），回答：权威来源、分支/合并、push/pull 时机、
      凭据、离线写入、敏感信息扫描、跨机权限。
- [ ] 推荐拓扑：远端裸仓库为权威源；每台机器本地 clone；每会话 linked worktree；
      每 agent/session 分支；显式 promote/merge，不自动合并冲突。
- [ ] 实现 clone/remote 配置、push/pull、冲突回报；凭据走 agenix，不进仓库。
- [ ] 测试：两台机器并发写同一记忆；离线提交后同步；冲突可定位、可回滚。
- 依据 feedback 6；依赖 T2。
- 验收：跨机共享有明确操作流程和失败通知，不依赖“某个 worktree 恰好是主仓库”。

### T10. 扩展工厂（ADR-0029 P4）

- [ ] authoring tools：scaffold / validate / build / install / reload / rollback。
- [ ] 各 loader 暴露工具链：Lua（校验/重载）、process（协议/监管）、native（v2 构建/ABI 检查）。
- [ ] 扩展源码 git 流程：会话 worktree 编写 → 校验 → promote 到 active checkout → 热加载。
- [ ] 审批与审计：T1/T2 模块安装、扩权、native 构建都留痕。
- [ ] 示例：让 AI 从规格生成一个 route 模块并跑通一轮。
- 依据：ADR-0029 扩展工厂；依赖 T4/T6/T7。
- 验收：AI 能在不重启桥的情况下完成“写模块 → 校验 → 安装 → 实测 → 回滚”。

## 2. 建议提交顺序

1. T1 `fix(note): term/member status contract`
2. T2 `fix(memory): linked worktree and push semantics`
3. T3 `fix(turn): late send window after turn end`
4. T4 `feat(native-abi): unload-safe plugin v2`
5. T5 `docs(examples): nested manager sample`
6. T6 `feat(dynamic): authoring and turn-boundary reload`
7. T7 `feat(task): result records and wake bridge`
8. T8 `feat(feedback): lifecycle and reply wake`
9. T9 `feat(memory): remote topology`
10. T10 `feat(factory): extension authoring toolchain`
