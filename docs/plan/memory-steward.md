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

## 3. 一次同步做什么

对每个目标（会话 × 记忆）：

1. worktree 不存在 → 跳过（worktree 由 `MemoryRepo` 惰性创建）。
2. `git status --porcelain` 非空 → 记 `dirty`，不碰工作区。
3. 取当前分支（`git rev-parse --abbrev-ref HEAD`）。
4. 没有 `origin` → 记 `no_remote`。
5. `git fetch --prune origin`。
6. 远端还没有同名分支 → 直接把本地分支推上去。
7. `git rev-list --left-right --count HEAD...origin/<branch>`：
   - `behind=0`：有本地提交就 push，否则 `ok`；
   - `behind>0`：按配置做 `merge` 或 `rebase`。
8. 合并成功 → push；冲突 → 见下一节。

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
2. `git merge --abort` / `git rebase --abort`，工作区回到同步前；
3. 状态记 `conflict` + `conflict_files`；
4. 通过通知钩子把文件列表交给会话；
5. 下一次按失败重试下限再次尝试，直到人工处理。

**默认绝不取一侧**，本地提交和远端提交都还在，不产生任何「看起来成功」的合并提交。

### 自动策略（必须显式开启）

```toml
# auto_resolve = "local"   # 冲突文件保留本地一侧
# auto_resolve = "remote"  # 冲突文件保留远端一侧
```

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

`EventQueue::submit` 是非阻塞的：队列满/关闭只打一行日志，状态文件照常写。
因此「通知失败」不会反过来拖住同步，也不会拖住轮次。

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
| `last_result` | `ok` / `pushed` / `merged` / `rebased` / `conflict` / `dirty` / `no_remote` / `remote_unreachable` / `error` |
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
- 通知只写该会话的第一条订阅房间；没有订阅的会话只能从 `status.json` /
  `/api/memory-steward` 看到状态。
- git 凭据完全交给 git 自身的 credential helper / SSH 配置；管家设置
  `GIT_TERMINAL_PROMPT=0`，不会在后台卡住等待输入。

## 9. 测试

`crates/memory-steward/tests/sync.rs`（临时 git 仓库，不依赖网络）：

- `schedule_combines_time_and_commit_thresholds`：时间/提交量单独与组合触发；
- `commit_threshold_triggers_a_sync_and_push`：两次提交触发一次同步并推到远端；
- `time_interval_triggers_even_without_new_commits`：没有新提交也按时间触发；
- `conflict_stops_without_losing_the_local_side`：默认策略下冲突停下、列出文件、
  本地 HEAD 与工作区保持原样、通知会话、状态里冲突数为 1；
- `auto_resolve_is_opt_in`：默认配置不会自动解决；
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
auto_resolve = "none"      # none（默认，冲突停下）| local | remote
state_dir = "state/memory-steward"
git_timeout_secs = 120
```

`enabled = false`（默认）时不起线程、不建状态目录；配置仍会在启动期校验。
