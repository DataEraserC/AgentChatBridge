// 通用 ACP 适配器的「agent 声明」层。
//
// 为什么要有这一层：ACP（Agent Client Protocol）是标准化的「stdio JSON-RPC」，
// 谁都能实现——codebuddy-acp、未来的其它 agent、甚至一个包装脚本。适配器如果
// 把「怎么启动」写死成某个安装布局（比如 `bin/workbuddy` 旁边一定有
// `opt/WorkBuddy/resources/.../codebuddy-headless.js`），每换一个 agent 就要改代码。
//
// 这里改成**声明式**：启动命令是一份不透明的 argv，适配器只负责按声明的 argv
// spawn，再讲 ACP。它不拆参数、不认包装器语义、不在别的参数里扫描 agent 名，
// 也不要求 GUI 用 multi-instance 还是直接启动——那两件事都在 ACB 之外。
//
// 形状刻意和 webmux 的 `WEBMUX_ACP_AGENTS` 保持一致（同一份 JSON 两边都能吃）：
//
//   {
//     "codebuddy": {
//       "title": "CodeBuddy",
//       "processExecutables": ["codebuddy"],
//       "commands": [
//         { "match": ["codebuddy", "--acp"], "acp": ["codebuddy-acp", "--acp"] }
//       ]
//     }
//   }
//
// 对 ACB 来说只有 `commands[].acp` 会被执行：ACB 没有要接管的 TUI，所以 `match`
// 和 `launcher` 只做形状校验、不参与匹配；恢复会话走 ACP 的 `session/load`，
// 不用 `resume` 的 argv。保留这些字段是为了「同一份声明可以原样复用」。

import fs from "node:fs"
import path from "node:path"

/** 声明文档所在的环境变量。 */
export const AGENTS_ENV = "ACB_ACP_AGENTS"
/** 本进程服务哪个 agent（稳定 id）。 */
export const AGENT_ENV = "ACB_ACP_AGENT"
/** 单 agent 的快捷方式：直接给 ACP 的 argv（JSON 数组）。 */
export const COMMAND_ENV = "ACB_ACP_COMMAND"

/** 上限都照着「防止一个坏声明把进程拖死」定，不是功能限制。 */
const MAX_DOC_BYTES = 4 << 20
const MAX_AGENTS = 100
const MAX_ID_BYTES = 64
const MAX_COMMANDS = 100
const MAX_ARGV_ITEMS = 128
const MAX_ARG_BYTES = 8192

const ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/

export class AcpConfigError extends Error {
  constructor(message) {
    super(message)
    this.name = "AcpConfigError"
    this.kind = "config"
  }
}

/**
 * 一条 agent 声明校验后的样子。
 *
 * `acp` 是**完整 argv**：第一个元素是可执行文件，其余是参数，不经过 shell。
 */
export function normalizeAgent(id, spec) {
  if (typeof id !== "string" || !ID_PATTERN.test(id) || Buffer.byteLength(id) > MAX_ID_BYTES) {
    throw new AcpConfigError(`agent id ${JSON.stringify(id)} 不合法（字母数字开头，只含 . _ -，≤64 字节）`)
  }
  if (!spec || typeof spec !== "object" || Array.isArray(spec)) {
    throw new AcpConfigError(`agent ${id} 的声明必须是对象`)
  }
  if (spec.title !== undefined && typeof spec.title !== "string") {
    throw new AcpConfigError(`agent ${id}: title 必须是字符串`)
  }
  if (spec.processExecutables !== undefined) {
    if (!Array.isArray(spec.processExecutables)) {
      throw new AcpConfigError(`agent ${id}: processExecutables 必须是字符串数组`)
    }
    for (const name of spec.processExecutables) {
      if (typeof name !== "string" || name === "") {
        throw new AcpConfigError(`agent ${id}: processExecutables 里有非字符串项`)
      }
    }
  }
  if (!Array.isArray(spec.commands) || spec.commands.length === 0) {
    throw new AcpConfigError(`agent ${id}: commands 必须是非空数组`)
  }
  if (spec.commands.length > MAX_COMMANDS) {
    throw new AcpConfigError(`agent ${id}: commands 超过 ${MAX_COMMANDS} 条`)
  }

  const commands = spec.commands.map((command, index) => {
    if (!command || typeof command !== "object" || Array.isArray(command)) {
      throw new AcpConfigError(`agent ${id}: commands[${index}] 必须是对象`)
    }
    return {
      // ACB 不跑 match / launcher / resume；只校验形状，坏形状仍然报出来。
      match: argvOf(id, index, "match", command.match, { optional: true }),
      acp: argvOf(id, index, "acp", command.acp),
      resume: argvOf(id, index, "resume", command.resume, { optional: true }),
      launcher: optionalString(id, index, "launcher", command.launcher),
      connector: command.connector ?? null,
    }
  })

  return {
    id,
    title: spec.title ?? id,
    processExecutables: spec.processExecutables ?? [],
    commands,
    /** 实际会被 spawn 的那条 argv（第一条带 acp 的声明）。 */
    acp: commands[0].acp,
  }
}

function optionalString(id, index, field, value) {
  if (value === undefined || value === null) return ""
  if (typeof value !== "string") {
    throw new AcpConfigError(`agent ${id}: commands[${index}].${field} 必须是字符串`)
  }
  return value
}

function argvOf(id, index, field, value, { optional = false } = {}) {
  if (value === undefined || value === null) {
    if (optional) return []
    throw new AcpConfigError(`agent ${id}: commands[${index}].${field} 缺失`)
  }
  if (!Array.isArray(value) || value.length === 0) {
    if (optional) return []
    throw new AcpConfigError(`agent ${id}: commands[${index}].${field} 必须是非空 argv 数组`)
  }
  if (value.length > MAX_ARGV_ITEMS) {
    throw new AcpConfigError(`agent ${id}: commands[${index}].${field} 参数太多（>${MAX_ARGV_ITEMS}）`)
  }
  return value.map((item, at) => {
    if (typeof item !== "string") {
      throw new AcpConfigError(`agent ${id}: commands[${index}].${field}[${at}] 不是字符串`)
    }
    if (Buffer.byteLength(item) > MAX_ARG_BYTES) {
      throw new AcpConfigError(`agent ${id}: commands[${index}].${field}[${at}] 太长（>${MAX_ARG_BYTES} 字节）`)
    }
    return item
  })
}

/**
 * 解析一份声明文档。坏条目**跳过并记账**，不拖垮整份文档——一个笔误不该让
 * 已经能用的 agent 全部下线（和 webmux 的处理一致）。
 */
export function parseAgents(raw) {
  const errors = []
  if (typeof raw !== "string" || raw.trim() === "") return { agents: [], errors }
  if (Buffer.byteLength(raw) > MAX_DOC_BYTES) {
    return { agents: [], errors: [`${AGENTS_ENV} 超过 ${MAX_DOC_BYTES} 字节，整份忽略`] }
  }
  let doc
  try {
    doc = JSON.parse(raw)
  } catch (error) {
    return { agents: [], errors: [`${AGENTS_ENV} 不是合法 JSON：${error.message}`] }
  }
  if (!doc || typeof doc !== "object" || Array.isArray(doc)) {
    return { agents: [], errors: [`${AGENTS_ENV} 顶层必须是对象`] }
  }
  const ids = Object.keys(doc).sort()
  if (ids.length > MAX_AGENTS) {
    return { agents: [], errors: [`${AGENTS_ENV} 声明了 ${ids.length} 个 agent，超过上限 ${MAX_AGENTS}`] }
  }
  const agents = []
  for (const id of ids) {
    try {
      agents.push(normalizeAgent(id, doc[id]))
    } catch (error) {
      errors.push(error.message)
    }
  }
  return { agents, errors }
}

/** 把 `ACB_ACP_COMMAND`（JSON argv）解析成单条临时声明。 */
export function agentFromCommand(raw, id = "acp") {
  let argv
  try {
    argv = JSON.parse(raw)
  } catch (error) {
    throw new AcpConfigError(`${COMMAND_ENV} 必须是 JSON 数组：${error.message}`)
  }
  if (!Array.isArray(argv) || argv.length === 0) {
    throw new AcpConfigError(`${COMMAND_ENV} 必须是非空 JSON 数组`)
  }
  return normalizeAgent(id, { title: id, commands: [{ acp: argv }] })
}

/**
 * 从环境里挑出**本进程要服务的那个 agent**。
 *
 * 规则（顺序即优先级）：
 * 1. 有 `ACB_ACP_AGENTS`：按 `ACB_ACP_AGENT` 选；只声明了一个时不必再指定；
 * 2. 没有声明但有 `ACB_ACP_COMMAND`：用它当单 agent；
 * 3. 都没有/选不出来：报错，并把可用 id 列出来——不猜。
 */
export function resolveAgent(env = process.env) {
  const { agents, errors } = parseAgents(env[AGENTS_ENV] ?? "")
  const wanted = (env[AGENT_ENV] ?? "").trim()
  const notify = errors.length ? { warnings: errors } : {}

  if (agents.length > 0) {
    if (wanted) {
      const found = agents.find((agent) => agent.id === wanted)
      if (!found) {
        throw new AcpConfigError(
          `${AGENT_ENV}=${wanted} 不在声明里；可用：${agents.map((agent) => agent.id).join(", ")}`,
        )
      }
      return { ...found, ...notify }
    }
    if (agents.length === 1) return { ...agents[0], ...notify }
    throw new AcpConfigError(
      `声明了多个 agent（${agents.map((agent) => agent.id).join(", ")}），请用 ${AGENT_ENV} 指定`,
    )
  }

  const command = (env[COMMAND_ENV] ?? "").trim()
  if (command) {
    return {
      ...agentFromCommand(command, (env.ACB_HARNESS ?? "").trim() || "acp"),
      ...notify,
    }
  }

  throw new AcpConfigError(
    `没有可用的 ACP agent 声明：设置 ${AGENTS_ENV}（JSON）或 ${COMMAND_ENV}（argv JSON 数组）`,
  )
}

/**
 * 可执行文件的「在不在」只在报错时用来提示，不参与决策：
 * 命令是声明出来的，PATH 解析交给 spawn。
 */
export function describeCommand(argv) {
  if (!Array.isArray(argv) || argv.length === 0) return "(空命令)"
  const program = argv[0]
  const looksAbsolute = program.includes("/")
  const hint = looksAbsolute
    ? fs.existsSync(program)
      ? ""
      : "（文件不存在）"
    : `（按 PATH 解析${path.isAbsolute(program) ? "" : ""}）`
  return `${argv.join(" ")}${hint}`
}
