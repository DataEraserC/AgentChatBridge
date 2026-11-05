# 记忆管家：后台同步、调度与冲突

状态：**设计 + 最小实现（2026-11-05）**。实现落在独立 crate `acb-memory-steward`，
由 `acb-bridge` 在启动时拉一个后台线程；**不进入 core 的轮次路径**。

## 1. 边界

记忆是一个 git 仓库：裸仓库 + 每个「会话 × 记忆」一个 linked worktree，路径固定为
`worktrees_root/<记忆id>/<会话id>`，分支 `acb/<会话id>`。本地提交由 agent 通过
`memory_git` 工具完成；管家只负责把本地提交推上去、把远端提交合进来。

明确不做：

- 多机拓扑发现、节点选举；
- CRDT、逐字段合并；
- 跨仓库引用；
- 在 core 的 `ingest` / `open_turn` / 投递路径里同步任何东西。

远端就是一个普通 git remote（`origin`）。没有 `origin` 的 worktree 不报错，只记
`no_remote` 状态。

## 2. 调度接口

调度器是 trait，不是 core 里的定时器：

```rust
pub trait Scheduler: Send + Sync {
    fn due(&self, state: &SyncState, now_ms: i64, commits_since_sync: u64) -> bool;
}
```

内置实现 `CombinedSchedule`，两个触发器可单独开、也可组合（**先到者触发**）：

```toml
[memory_steward]
enabled = true
interval_secs = 300       # 时间触发：距上次尝试满 300s；0 = 关闭
commit_threshold = 5      # 提交量触发：本地自上次同步后新增 5 个提交；0 = 关闭
```

- 时间触发看 `SyncState::last_attempt_ms`（失败也推进，避免每个 poll 都打远端）。
- 提交量触发看 `git rev-list --count <last_head>..HEAD`。
- 两者都为 0 且 `enabled = true` 在启动解析期报错，不允许「开着但永不触发」。
- `poll_secs`（默认 30）只是后台线程的检查间隔，不是同步周期。
- 失败/冲突后的重试下限是 `max(interval_secs, poll_secs)`，防止远端不可达时每个
  poll 都发起一次 fetch。这是实现层的退避，不是第三个调度触发器。

扩展新触发方式（例如「远端有未读通知」）：实现 `Scheduler`，在
`StewardConfig::scheduler()` 里换成新实现即可；`Steward::run_once` 只依赖 trait。

首次见到某个 worktree 时先建立基线 `last_head = 当前 HEAD` 并落盘，所以历史提交
不会在第一次运行时被误判成「新增提交」。

## 3. 一次同步做什么（演练与落地分离）

对每个目标（会话 × 记忆）：

1. worktree 不存在 → 跳过（worktree 由 `MemoryRepo` 惰性创建）。
2. 主工作区 `git status --porcelain` 非空 → 记 `dirty`，不碰它。
3. 记录主工作区 `HEAD` 与指纹（HEAD + status + 文件相对路径/大小/mtime/内容哈希）。
4. `git worktree add --detach state_dir/memory-merge/<记忆>/<会话> <HEAD>` 建**临时演练 worktree**；主工作区在这期间一动不动。
5. 在演练 worktree 里 `git fetch --prune origin`。
6. 取主工作区分支名；远端没有同名分支 → 收掉演练 worktree，直接 push 主工作区。
7. 在演练 worktree 里 `git rev-list --left-right --count HEAD...origin/<branch>`：
   - `behind=0`：有本地提交就 push 主工作区，否则 `ok`；收掉演练 worktree；
   - `behind>0`：在演练 worktree 里 `merge` 或 `rebase`。
8. 冲突 → 在演练 worktree 里 `merge --abort` / `rebase --abort`，主工作区完全不受影响。
9. 演练结果干净 → 落地前再比一次主工作区 `HEAD` / 指纹 / `git status`；任何不一致记 `concurrent_write`，**放弃本次落地**，不覆盖。
10. 落地：拿 `acb-kernel::memory::worktree_write_lock`（与 core 的 `memory_write` / `git_add` / `git_commit` 同一把），在锁内再检查一次，然后 `git reset --hard <演练结果>`；失败尝试回滚到旧 HEAD。锁只覆盖这一小段，不覆盖演练。
11. 收掉演练 worktree（`Drop` 兜底，无论成功失败），push 主工作区。

启动时 `Steward::new` 会清理 `state_dir/memory-merge/*` 残留：配置里还在的目标走
`git worktree remove --force` + `prune`，配置里已经删掉的目录直接删掉。

所有 git 命令都有 `git_timeout_secs`（默认 120s）上限，超时会 kill 子进程并按失败
记录。`GIT_TERMINAL_PROMPT=0`，不会卡在凭据输入。

## 4. 合并与冲突策略

### 合并策略

```toml
merge_strategy = "merge"   # 默认；保留分叉历史
# merge_strategy = "rebase"  # 可选；把本地提交重放到远端之上
```

### 冲突默认行为

`auto_resolve = "none"`（默认）：

1. `git diff --name-only --diff-filter=U` 收集冲突文件；
2. 在**演练 worktree** 里 `git merge --abort` / `git rebase --abort`；主工作区从未被触碰；
3. 状态记 `conflict` + `conflict_files`；
4. 通过通知钩子把文件列表交给会话；
5. 下一次按失败重试下限再次尝试，直到人工处理。

**默认绝不取一侧**，本地提交和远端提交都还在，不产生任何「看起来成功」的合并提交。

### 自动策略（必须显式开启，会丢数据）

```toml
# auto_resolve = "local"   # 冲突文件整体取本地：远端在这些文件里的改动被丢弃
# auto_resolve = "remote"  # 冲突文件整体取远端：本地在这些文件里的改动被丢弃
```

`local` / `remote` **不是内容级自动合并**，而是按文件整体取一侧：另一侧在冲突文件
里的改动会被丢弃。默认 `none` 就是因为这个策略会丢数据；要开必须在配置里显式写。

注意 merge 与 rebase 的 `--ours` / `--theirs` 语义相反，实现里做了映射：

| 配置 | `merge` | `rebase` |
|---|---|---|
| `local` | `--ours` | `--theirs` |
| `remote` | `--theirs` | `--ours` |

自动策略只处理「文件粒度取一侧」：逐个冲突文件 `checkout <side>` + `git add`，
merge 走 `git commit --no-edit`，rebase 走 `rebase --continue`（最多 10 轮，
不收敛就 abort 并记 `error`）。它不做内容级合并，也不猜语义。

## 5. 通知与不阻塞

管家在 `acb-bridge` 里以一个独立线程运行（`Steward::run_forever`），每
`poll_secs` 检查一次调度。同步、git 子进程、状态写盘都发生在这个线程里；agent 的
对话轮次不等待它。

失败/冲突的通知走**入站队列**，不直接锁 `Bridge`：

- 事件名：`memory_sync_conflict` / `memory_sync_failed`；
- sender：`acb.memory-steward`；
- tags：`memory_sync` + `conflict` / `failure`；
- 写入该会话的**第一条订阅房间**；会话没有订阅时只记状态和日志；
- `at_self = true`，`at_mention` 的唤醒策略会看到它；不想被叫醒的会话可以配
  `ignore_event_tags = ["memory_sync"]`。

**为什么只投给这个会话，而不是广播给所有使用该记忆的会话**：每个「会话 × 记忆」
有独立的 worktree 和分支 `acb/<会话id>`；冲突只发生在这个会话的分支上，别的会话
有自己的分支和本地提交，广播只会让无关 agent 去碰别人的记忆。控制台/状态文件始终
能看到所有目标的冲突，运维可以在那里统一处理。

`EventQueue::submit` 是非阻塞的：队列满/关闭只打一行日志，状态文件照常写。
因此「通知失败」不会反过来拖住同步，也不会拖住轮次。落地锁只覆盖「检查 + reset」
这一小段，不覆盖 fetch/merge 演练；agent 的 `memory_write` / `git_add` / `git_commit`
在锁上等待的时间是毫秒级，不是整轮。

## 6. 状态与可观测

状态文件默认 `state/memory-steward/status.json`（原子替换：先写 `.json.tmp` 再
rename）。每条目标一个键：`<记忆id>/<会话id>`。

```json
{
  "version": 1,
  "updated_at_ms": 1760000000000,
  "targets": {
    "m1/s1": {
      "last_attempt_ms": 1760000000000,
      "last_success_ms": 1759999700000,
      "last_result": "conflict",
      "conflict_files": ["shared.md"],
      "error": "merge conflict; stopped without choosing a side (auto_resolve is none)",
      "last_head": "..."
    }
  }
}
```

| 字段 | 含义 |
|---|---|
| `last_attempt_ms` | 最近一次尝试同步的时间（成功、失败、冲突都算） |
| `last_success_ms` | 最近一次成功推送/合并的时间 |
| `last_result` | `ok` / `pushed` / `merged` / `rebased` / `conflict` / `dirty` / `no_remote` / `remote_unreachable` / `concurrent_write` / `landing_failed` / `error` |
| `conflict_files` | 冲突文件列表；**冲突文件数 = 该列表长度**（不额外存冗余计数字段） |
| `error` | 失败原因（fetch/push/merge/超时等原文摘要） |
| `last_head` | 上次成功同步的 HEAD，用于计算新增提交数 |

查询：

- 文件：`state/memory-steward/status.json`（路径可通过 `state_dir` 改）；
- 控制台：`GET /api/memory-steward`（只读该文件；未启用返回
  `{"enabled": false}`）。

## 7. 失败模式

| 场景 | 状态 | 数据 | 通知 | 行为 |
|---|---|---|---|---|
| 远端不可达 / fetch 失败 | `remote_unreachable` | 本地提交不动 | `memory_sync_failed` | 记 `error`；按重试下限再试；不阻塞轮次 |
| 默认策略下冲突 | `conflict` | abort 合并，两侧提交都在 | `memory_sync_conflict` | 停下等人工；下一次按重试下限再试 |
| 自动策略解析失败 | `error` | abort，冲突文件保留 | `memory_sync_failed` | 不用半成品合并提交；下一次再试 |
| 演练期间主工作区被外部写入/提交 | `concurrent_write` | 主工作区原样保留，外部写入不覆盖 | `memory_sync_failed`（reason 含 `concurrent_write`） | 放弃本次落地；下一轮主工作区脏则继续跳过，等人提交/清理 |
| 落地 `reset --hard` 失败 | `landing_failed` | 尝试回滚到旧 HEAD；回滚失败会在 `error` 里注明 | `memory_sync_failed` | 不把半落地当成功；下一次重新演练 |
| 工作区有未提交改动 | `dirty` | 不动 | 无 | 跳过本次；提交后自然恢复 |
| 没有 `origin` | `no_remote` | 不动 | 无 | 跳过；配置远端后恢复 |
| 非 fast-forward 的 push 被拒 | `error` | 本地提交保留 | `memory_sync_failed` | 下一轮先 fetch + 合并再推 |
| git 命令超时 | `error` | 本地提交保留 | `memory_sync_failed` | kill 子进程；下一轮再试 |
| 状态文件写失败 | 内存态继续 | 不影响 git | 无 | 打日志；下一次写盘重试 |
| 入站队列满 / 已关闭 | 状态照常写 | 不影响 git | 丢失本次通知 | 只打日志；不阻塞同步 |
| worktree 不存在 | 无记录 | — | 无 | 跳过（等 `MemoryRepo` 创建） |
| 桥重启 | 从 `status.json` 恢复 | git 仓库不受影响 | 无 | 基线 `last_head` 不丢，不会把历史提交当新增 |

## 8. 已知边界

- 管家读取**启动时**的 `[memory_steward]` 配置；控制台热重载配置不会重启管家线程，
  改了开关/调度参数需要重启桥。
- worktree 由 `MemoryRepo` 在会话第一次使用记忆时惰性创建；管家只同步已存在的
  worktree，不会替会话创建。
- 通知只写该会话的第一条订阅房间；原因见 §5。没有订阅的会话只能从 `status.json` /
  `/api/memory-steward` 看到状态。
- 落地锁是 `acb-kernel::memory::worktree_write_lock`，按 worktree 一把；core 的
  `write_file` / `git_add` / `git_commit` 也拿同一把。`git_push` 不在锁内（它不改
  本地工作区）。
- `state_dir` 必须在记忆 worktree **之外**（默认 `state/memory-steward` 就满足）；
  如果把 `state_dir` 放进某个记忆 worktree，演练目录会被当成未跟踪文件，导致每次
  都 `concurrent_write`。
- 外部写入检测覆盖 HEAD、`git status`、文件相对路径/大小/mtime/内容哈希（≤4 MiB
  的文件读内容，大文件只看元数据）。检测到 `concurrent_write` 后主工作区可能变成
  `dirty`，下一轮会按 `dirty` 跳过，直到人提交或清理。
- git 凭据完全交给 git 自身的 credential helper / SSH 配置；管家设置
  `GIT_TERMINAL_PROMPT=0`，不会在后台卡住等待输入。

## 9. 测试

`crates/memory-steward/tests/sync.rs`（临时 git 仓库，不依赖网络）：

- `schedule_combines_time_and_commit_thresholds`：时间/提交量单独与组合触发；
- `commit_threshold_triggers_a_sync_and_push`：两次提交触发一次同步并推到远端；
- `time_interval_triggers_even_without_new_commits`：没有新提交也按时间触发；
- `conflict_stops_without_losing_the_local_side`：默认策略下冲突停下、列出文件、
  主工作区 `HEAD` 与 `git status` 原样、演练 worktree 收掉、通知会话、状态里冲突数为 1；
- `auto_resolve_is_opt_in`：默认配置不会自动解决；
- `external_write_during_rehearsal_is_not_overwritten`：merge 期间通过 git hook 往
  主工作区注入外部写入，结果必须 `concurrent_write`、主 HEAD 不变、外部文件不被覆盖、
  通知送达、演练 worktree 收掉；
- `stale_merge_worktree_is_cleaned_on_start`：启动时清掉 `state_dir/memory-merge` 残留
  目录与 admin 元数据；
- `landing_failure_rolls_back_to_old_head`（lib 单测）：落地失败回到旧 HEAD，主工作区
  保持干净；
- `remote_unreachable_only_records_status`：远端不可达时本地提交不丢、状态可查、
  通知失败、单次同步不被拖死；
- `merge_strategy_defaults_to_merge_and_auto_resolve_is_explicit`：默认值与 TOML
  解析、非法配置（两个触发器都为 0）报错。

## 10. 配置示例

```toml
[memory_steward]
enabled = true
interval_secs = 300        # 5 分钟兜底
commit_threshold = 5       # 或 acb-agent 提交 5 次后先触发
poll_secs = 30
merge_strategy = "merge"   # merge | rebase
auto_resolve = "none"      # none（默认，冲突停下）| local / remote（会丢弃另一侧）
state_dir = "state/memory-steward"
git_timeout_secs = 120
```

`enabled = false`（默认）时不起线程、不建状态目录；配置仍会在启动期校验。
