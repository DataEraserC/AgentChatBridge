#!/usr/bin/env node
// dsh_defaults_guard.mjs —— 检出/修复「ACB 的 preset 变成整个 profile 的默认 preset」。
//
// 背景（已取证，见 docs/plan/todo-ledger.md 对应小节）：
//   dsh 有三个层次会记「默认值」，只有最后一层真正决定新会话用什么：
//     1. profiles/<profile>/cordis.patch.yml —— nix 快照层，被 dsh-seed-profile
//        在启动时整份覆盖，所以它不是权威，改了也会被抹掉；
//     2. $DSH_HOME/settings-persist/<profile>/auto.json —— dsh-settings-persist
//        插件在每次 Settings 保存时 adopt 的运行态副本，**启动时重放**；
//     3. 同目录 snapshots/*.json —— 用户手动快照，只在用户点「恢复」时生效。
//   y 也就是说：想让「默认值」不再被改，光改 (1) 没用；要改的是 (2)。
//
// 本守卫只做一件窄事，且只用磁盘上能证明的事实归类：
//   - selectedDefault 指向 `acb-*`（本仓库 acbctl sessions 推导出的动态 preset id，
//     见 dsh_presets_acb.sh 的 BUNDLE_PREFIX / acb-<session>）：**只报告，不修**。
//
//     为什么不再自动改回：2026-11-16 复核确认，`selectedDefault` 在整个 dsh 里
//     只有一个**写入点**——浏览器设置页的人工点击
//     （`dsh-client-ui-agent-preset/lib/client.js:1095` 的 `remote.settings.update`，
//     由卡片 `onClick → makeDefault` 触发）。registry 侧只有只读配置
//     （`z.string().volatile()` + `.get() ?? default`），没有任何进程会写它。
//     也就是说：**磁盘上出现 acb-* 几乎只能是人手动选的**（用户 2026-11-16 也确认
//     「可能确实是人工修改的」）。既然是人的选择，程序去改回就等于覆盖用户设置——
//     那正是用户抱怨的那类行为。本守卫因此降级为**只读诊断 + 打印建议命令**。
//   - selectedDefault 指向其它值（standard/minimal/ptc/自定义）：那是人的选择，
//     只报告，永不改写。
//   - agent-default-model 的 provider/model：dsh 的 session/selectModel 会**无条件**
//     把会话选择写成全局默认（dsh-api-session-controller 的 selectModel：注释原文
//     “save the default in the background”），而 ACB driver 每个会话都要钉一次模型。
//     但磁盘上无法证明当前值是谁写的（人也可能自己改过），所以**只报告、不改写**，
//     归类 model-default-drift / cause=unattributed。
//
// 姿态：**只读**。守卫发现可疑值就打印出来并给建议命令，改不改由人决定。
// 一个会静默改写用户设置的启动钩子，比它想修的问题更糟。
//
// 退出码：0 干净（或已修复）；3 发现需要人处理的漂移（检查模式）；2 用法/环境错误。

import { readFileSync, existsSync, readdirSync } from 'node:fs'
import path from 'node:path'

const EXIT_OK = 0
const EXIT_USAGE = 2
const EXIT_DRIFT = 3

const USAGE = `用法：dsh_defaults_guard.mjs --dsh-home DIR [--profile NAME] [--json]

  --dsh-home DIR   DSH_HOME（$DSH_HOME）
  --profile NAME   profile 名（默认 nix-web-acb，与 dsh_nix.sh 的 ACB_DSH_PROFILE 一致）
  --json           机器可读输出

本守卫**只读**：不写任何文件。它打印建议命令，改不改由人决定。
退出码：0 干净；3 有需要人看一眼的漂移；2 用法错误`

/** acbctl sessions 推导出的动态 preset id：`acb-<session id>`。 */
const ACB_PRESET = /^acb-/

function die(message) {
  console.error(`dsh-defaults-guard: ${message}`)
  process.exit(EXIT_USAGE)
}

function parseArgs(argv) {
  const options = { dshHome: '', profile: 'nix-web-acb', json: false }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    if (arg === '--dsh-home') {
      options.dshHome = argv[++index] ?? die('--dsh-home 需要一个值')
    } else if (arg === '--profile') {
      options.profile = argv[++index] ?? die('--profile 需要一个值')
    } else if (arg === '--apply') {
      // 旧参数：曾经用于自动改写 auto.json。现在明确拒绝，避免有人照着旧文档
      // 执行却发现“什么也没发生”。改写人的设置不该由脚本代劳。
      die('--apply 已移除：本守卫只读。selectedDefault 是人手选的，程序不改写它。')
    } else if (arg === '--json') {
      options.json = true
    } else if (arg === '-h' || arg === '--help') {
      console.log(USAGE)
      process.exit(EXIT_OK)
    } else {
      die(`未知参数：${arg}`)
    }
  }
  if (!options.dshHome) die('缺少 --dsh-home（用 $DSH_HOME）')
  return options
}

function readText(file) {
  try {
    return readFileSync(file, 'utf8')
  } catch {
    return null
  }
}

/**
 * 从 YAML 文本里取 `- id: <entryId>` 这一块里的标量字段。
 *
 * 只做块扫描，不引入 YAML 依赖：dsh 的 profile patch 是「顶层序列 + 每项
 * id/name/config」这一种形状，块边界是下一个行首 `- id: `。拿不到就返回
 * undefined（调用方按「字段不存在」处理，不猜）。
 *
 * @param {string} text   YAML 文本（可以是 auto.json 里的 documentText）。
 * @param {string} entryId 目标条目 id，例如 `agent-preset-registry`。
 * @param {string} field   块内的字段名，例如 `selectedDefault`。
 * @returns {string|undefined} 去引号后的标量值。
 */
export function yamlBlockField(text, entryId, field) {
  const lines = String(text ?? '').split('\n')
  let inside = false
  for (const line of lines) {
    if (/^- id: /.test(line)) {
      const id = line.replace(/^- id: /, '').trim()
      inside = id === entryId
      continue
    }
    if (!inside) continue
    const match = new RegExp(`^\\s+${field}:\\s*(.*)$`).exec(line)
    if (match) {
      const raw = match[1].trim()
      if (raw === '' || raw === 'null' || raw === '~') return undefined
      return raw.replace(/^['"]/, '').replace(/['"]$/, '')
    }
  }
  return undefined
}

export function rewriteYamlBlockField(text, entryId, field, value) {
  const lines = String(text ?? '').split('\n')
  const out = []
  let inside = false
  let applied = false
  const pattern = new RegExp(`^(\\s+)${field}:\\s*.*$`)
  for (const line of lines) {
    if (/^- id: /.test(line)) {
      inside = line.replace(/^- id: /, '').trim() === entryId
      out.push(line)
      continue
    }
    if (!inside) {
      out.push(line)
      continue
    }
    const match = pattern.exec(line)
    if (!match) {
      out.push(line)
      continue
    }
    if (applied) {
      // 同名字段出现两次：只改第一处，第二处照原样留下（让 dsh 自己报重复）。
      out.push(line)
      continue
    }
    applied = true
    if (value === undefined) continue // 删除该行
    out.push(`${match[1]}${field}: ${value}`)
  }
  return out.join('\n')
}

function loadPatch(dshHome, profile) {
  const file = path.join(dshHome, 'profiles', profile, 'cordis.patch.yml')
  return { file, text: readText(file) }
}

/** 期望值只认 profile 快照（nix 真相）；快照里没有 = 期望「字段不存在」。 */
function expectedFromSnapshot(patchText) {
  return {
    selectedDefault: yamlBlockField(patchText, 'agent-preset-registry', 'selectedDefault'),
    defaultPreset: yamlBlockField(patchText, 'agent-preset-registry', 'default'),
    provider: yamlBlockField(patchText, 'agent-default-model', 'provider'),
    model: yamlBlockField(patchText, 'agent-default-model', 'model'),
  }
}

function loadAuto(dshHome, profile) {
  const file = path.join(dshHome, 'settings-persist', profile, 'auto.json')
  const raw = readText(file)
  if (raw === null) return { file, data: null }
  try {
    return { file, data: JSON.parse(raw) }
  } catch (error) {
    die(`auto.json 不是合法 JSON（${file}）：${error.message}`)
  }
}

function rowConfig(data, id) {
  const rows = Array.isArray(data?.rows) ? data.rows : []
  const row = rows.find((candidate) => candidate?.id === id)
  return row?.config
}

function countSnapshotDrift(dshHome, profile) {
  const dir = path.join(dshHome, 'settings-persist', profile, 'snapshots')
  if (!existsSync(dir)) return { dir, total: 0, acb: [] }
  let total = 0
  const acb = []
  for (const name of readdirSync(dir)) {
    if (!name.endsWith('.json')) continue
    total += 1
    const raw = readText(path.join(dir, name))
    if (raw === null) continue
    let state
    try {
      state = JSON.parse(raw)
    } catch {
      continue
    }
    const value = yamlBlockField(state?.documentText, 'agent-preset-registry', 'selectedDefault')
    if (value !== undefined && ACB_PRESET.test(value)) acb.push({ name, selectedDefault: value })
  }
  return { dir, total, acb }
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  const patch = loadPatch(options.dshHome, options.profile)
  // 没有 profile 快照就没有「期望值」，无从判断漂移：跳过而不是报错。
  // 测试 fixture 与未初始化 profile 都会走到这里。
  if (patch.text === null) {
    if (options.json) console.log(JSON.stringify({ profile: options.profile, skipped: 'no-profile-patch' }))
    else console.log(`dsh-defaults-guard: ${patch.file} 不存在，跳过`)
    process.exit(EXIT_OK)
  }
  const expected = expectedFromSnapshot(patch.text)
  const auto = loadAuto(options.dshHome, options.profile)
  const snapshots = countSnapshotDrift(options.dshHome, options.profile)

  const findings = []
  if (auto.data !== null) {
    const fromText = yamlBlockField(auto.data.documentText, 'agent-preset-registry', 'selectedDefault')
    const fromRows = rowConfig(auto.data, 'agent-preset-registry')?.selectedDefault
    for (const [where, value] of [['auto.documentText', fromText], ['auto.rows', fromRows]]) {
      if (typeof value === 'string' && ACB_PRESET.test(value)) {
        findings.push({
          kind: 'acb-preset-default',
          // 不再是 `defect`：selectedDefault 只有浏览器设置页一个人工写入点，
          // 磁盘上无法区分「ACB 造成的」与「人手选的」。判成缺陷会诱导人去
          // 「修掉」一个可能是正确的人工配置，所以按 `info` 报告、由人判断。
          severity: 'info',
          layer: where,
          cause: 'needs-human-review',
          value,
          expected: expected.selectedDefault ?? null,
        })
      } else if (typeof value === 'string' && value !== expected.selectedDefault) {
        findings.push({
          kind: 'user-preset-default',
          severity: 'info',
          layer: where,
          cause: 'user-chosen',
          value,
          expected: expected.selectedDefault ?? null,
        })
      }
    }
    const provider = rowConfig(auto.data, 'agent-default-model')?.provider
    const model = rowConfig(auto.data, 'agent-default-model')?.model
    if (provider !== undefined && expected.provider !== undefined) {
      if (provider !== expected.provider || model !== expected.model) {
        findings.push({
          kind: 'model-default-drift',
          // 人也能在 Settings 里改模型，磁盘上无法归因到 ACB：只记录，不判缺陷。
          // 真正阻止 ACB 写它的是 driver 的「钉模型前存、钉完还原」。
          severity: 'info',
          layer: 'auto.rows',
          cause: 'unattributed',
          value: `${provider}/${model ?? ''}`,
          expected: `${expected.provider}/${expected.model ?? ''}`,
        })
      }
    }
  }

  // 守卫只读：这里不写任何文件。上面收集到的 findings 就是全部输出。
  const remaining = findings
  // 只有 `defect` 是「不该发生、值得非零退出」的；`info` 是人自己的选择。
  // 「干净」= 没有任何条目值得人看一眼。既然 acb-* 也降级成 info，
  // 这里就必须按 findings 全空来判，否则那些条目会被 `clean` 分支吞掉、
  // 一个字都不打印——那等于悄悄忽略了一个可能是错配的默认值。
  const clean = remaining.length === 0 && snapshots.acb.length === 0

  if (options.json) {
    console.log(
      JSON.stringify(
        {
          profile: options.profile,
          clean,
          expected,
          findings,
          snapshots: { total: snapshots.total, acbDefault: snapshots.acb },
        },
        null,
        2,
      ),
    )
  } else if (clean) {
    console.log(`dsh-defaults-guard: ${options.profile} 没有 ACB 造成的默认值漂移`)
  } else {
    for (const finding of remaining) {
      const fix =
        finding.severity === 'info'
          ? '人的选择，只记录'
          : '只在人确认后才该改；本守卫不代劳'
      console.log(
        `dsh-defaults-guard: ${finding.kind} @ ${finding.layer} = ${finding.value}` +
          `（应为 ${finding.expected ?? '未设置'}；cause=${finding.cause}；${fix}）`,
      )
    }
    for (const snapshot of snapshots.acb) {
      console.log(
        `dsh-defaults-guard: 手动快照 ${snapshot.name} 也含 selectedDefault: ${snapshot.selectedDefault}` +
          '（只在用户点「恢复」时生效；恢复会把它重新灌回运行态）',
      )
    }
    console.log(
      'dsh-defaults-guard: 本守卫只读，没有改动任何文件。' +
        '要改就自己编辑该文件，或在 dsh 的 Settings 页重新选择默认值。',
    )
  }

  if (clean) process.exit(EXIT_OK)
  // 退出码只区分「有没有值得人看一眼的东西」。
  // 既然所有条目都降级成 info（磁盘上无法归因到 ACB），就不能再返回 3——
  // 调用方（dsh_nix.sh / dsh_presets_acb.sh）会把 3 当成警告打出来，
  // 而每次启动都告警的结果是**守卫被关掉**，比不装更糟。
  // 3 只留给真正的缺陷（当前没有这类条目）。
  process.exit(EXIT_OK)
}

main()
