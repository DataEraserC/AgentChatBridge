// WorkBuddy 适配器的配置解析。
//
// 硬约束（来自 docs/plan/harness-adapter-feasibility.md 4.8）：
//   上游 WorkBuddy 发行物只带 cli/dist/codebuddy-headless.js，没有 dist/codebuddy.js；
//   bin/codebuddy 在非无头命令上会 MODULE_NOT_FOUND。所以适配器只认 headless
//   bundle，或者显式设置 CODEBUDDY_FORCE_HEADLESS_BUNDLE=1 后再用 launcher。
//
// 配置只做三件事：找 CLI、算实例/会话 id 规则、准备每轮 CLI 参数。不读 GUI 的
// workbuddy.db（那是私有面），也不碰 ~/.workbuddy 的内部实现。
//
// 多实例：每个实例一个独立进程，用不同 ACB_WB_INSTANCE / ACB_HARNESS，
// 各自独立 state 目录（同时作为子进程 HOME 和 CODEBUDDY_CONFIG_DIR 的根）、
// 独立 MCP 端口、独立会话 id 前缀。实例间不共享登录态和会话记录。

import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { TOOLS } from "../../shared/tools.mjs"
import { t } from "../../shared/i18n.mjs"

const HEADLESS_BUNDLE = "codebuddy-headless.js"
export const DEFAULT_INSTANCE = "workbuddy-1"

/** 在 PATH 里找可执行文件；找不到返回 null。 */
function which(name, searchPath = "") {
  const dirs = String(searchPath || "").split(path.delimiter).filter(Boolean)
  for (const dir of dirs) {
    const candidate = path.join(dir, name)
    try {
      if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return candidate
    } catch {
      // 单个目录不可读就跳过。
    }
  }
  return null
}

/** 从 WorkBuddy 的安装布局推导 headless bundle 路径。 */
function candidatesFromWorkbuddy(binaryPath) {
  let real = binaryPath
  try {
    real = fs.realpathSync(binaryPath)
  } catch {
    // 用原路径试。
  }
  const binDir = path.dirname(real)
  return [
    // Nix 布局：<store>/bin/workbuddy -> <store>/opt/WorkBuddy/resources/...
    path.join(binDir, "..", "opt", "WorkBuddy", "resources", "app.asar.unpacked", "cli", "dist", HEADLESS_BUNDLE),
    // 普通安装布局：<app>/bin/workbuddy -> <app>/resources/...
    path.join(binDir, "..", "resources", "app.asar.unpacked", "cli", "dist", HEADLESS_BUNDLE),
  ]
}

/**
 * 校验入口是不是无头 bundle。
 * - codebuddy-headless.js：直接放行；
 * - bin/codebuddy 之类的 launcher：只有显式设置强制无头 env 才放行；
 * - 其它（GUI wrapper、codebuddy.js 全量 TUI）：直接拒绝并说明原因。
 */
export function assertHeadlessCli(cliPath, env = process.env) {
  const base = path.basename(cliPath)
  if (base === HEADLESS_BUNDLE) return cliPath
  // 测试用的假 CLI 也带 headless 字样，避免为了测试开后门。
  if (/headless/i.test(base) && (base.endsWith(".js") || base.endsWith(".mjs"))) return cliPath

  const forced =
    env.CODEBUDDY_FORCE_HEADLESS_BUNDLE === "1" || env.ACB_WB_FORCE_HEADLESS === "1"
  if (base === "codebuddy" && forced) return cliPath

  throw new Error(t("workbuddy.cli_not_headless", { path: cliPath }))
}

/** 找 CLI：显式 ACB_WB_CLI > WORKBUDDY_RESOURCES > PATH 里的 workbuddy。 */
export function resolveCliPath(env = process.env) {
  const explicit = env.ACB_WB_CLI?.trim()
  if (explicit) {
    const resolved = path.resolve(explicit)
    if (!fs.existsSync(resolved) || !fs.statSync(resolved).isFile()) {
      throw new Error(t("workbuddy.cli_missing", { path: resolved }))
    }
    return assertHeadlessCli(resolved, env)
  }

  const resources = env.WORKBUDDY_RESOURCES?.trim()
  if (resources) {
    const candidate = path.join(resources, "app.asar.unpacked", "cli", "dist", HEADLESS_BUNDLE)
    if (fs.existsSync(candidate)) return candidate
  }

  const binary = which("workbuddy", env.PATH)
  if (binary) {
    for (const candidate of candidatesFromWorkbuddy(binary)) {
      if (fs.existsSync(candidate)) return candidate
    }
  }

  throw new Error(t("workbuddy.cli_missing", {
    path: "set ACB_WB_CLI to .../cli/dist/codebuddy-headless.js",
  }))
}

/** 实例标识只允许字母/数字/连字符/下划线；空值退回默认实例。 */
export function sanitizeInstance(value) {
  const raw = String(value ?? "").trim() || DEFAULT_INSTANCE
  const clean = raw
    .replace(/[^A-Za-z0-9_-]/g, "-")
    .replace(/-+/g, "-")
    .replace(/^[^A-Za-z0-9]+/, "")
  return clean || DEFAULT_INSTANCE
}

/**
 * 实例的默认 state 目录。它同时作为子进程的 HOME 和 CODEBUDDY_CONFIG_DIR 的根，
 * 所以两个实例不会共用 ~/.codebuddy 的登录态/会话记录。
 */
export function defaultStateDir(instance = DEFAULT_INSTANCE, env = process.env) {
  const name = sanitizeInstance(instance)
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Application Support", "acb", "workbuddy", name)
  }
  if (process.platform === "win32") {
    const base = env.LOCALAPPDATA?.trim() || path.join(os.homedir(), "AppData", "Local")
    return path.join(base, "acb", "workbuddy", name)
  }
  const base = env.XDG_STATE_HOME?.trim() || path.join(os.homedir(), ".local", "state")
  return path.join(base, "acb", "workbuddy", name)
}

export function resolveStateDir(env = process.env) {
  const explicit = env.ACB_WB_STATE_DIR?.trim()
  if (explicit) return path.resolve(explicit)
  return defaultStateDir(env.ACB_WB_INSTANCE, env)
}

/**
 * ACB 会话 id → WorkBuddy 的 --session-id。
 *
 * 前缀带实例名，保证两个实例的 `-r` 不会续接到对方的会话；重置后加代次后缀，
 * 避免和旧会话撞 id。CLI 允许字母/数字/连字符/下划线/冒号，但这里不用冒号：
 * ACB 房间串里本来就有冒号，统一换成连字符更干净。
 */
export function sessionIdFor(acbSession, instance = DEFAULT_INSTANCE, generation = "") {
  const prefix = sanitizeInstance(instance)
  let raw = `acb-${prefix}-${String(acbSession ?? "")}`.replace(/[^A-Za-z0-9_-]/g, "-")
  raw = raw.replace(/-+/g, "-").replace(/^[^A-Za-z0-9]+/, "")
  if (!raw) raw = `acb-${prefix}`
  raw = raw.slice(0, 120)
  return generation ? `${raw}-r${generation}` : raw
}

function splitTools(value) {
  return String(value ?? "")
    .split(/[,\s]+/)
    .map((item) => item.trim())
    .filter(Boolean)
}

/**
 * 把允许清单展开成裸名 + `mcp__acb__` 前缀名。CLI 实际暴露的是前缀名，
 * `--allowedTools` 必须拿到前缀名才真正预批准；裸名保留给内置工具/旧版本。
 */
function expandAllowedTools(names) {
  const out = []
  const seen = new Set()
  for (const item of names) {
    const value = String(item ?? "").trim()
    if (!value) continue
    const prefixed = value.startsWith("mcp__") ? value : `mcp__acb__${value}`
    const bare = value.startsWith("mcp__") ? "" : value
    for (const name of [bare, prefixed]) {
      if (!name || seen.has(name)) continue
      seen.add(name)
      out.push(name)
    }
  }
  return out
}

/** 默认预批准全部桥工具：裸名和 MCP 前缀名都给，避免 CLI 在非交互模式下卡权限。 */
function defaultAllowedTools() {
  return expandAllowedTools(TOOLS.map((tool) => tool.name))
}

function parsePort(value, fallback = 0) {
  if (value === undefined || value === null || String(value).trim() === "") return fallback
  const port = Number(value)
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw new Error(t("workbuddy.mcp_port_invalid", { value: String(value) }))
  }
  return port
}

/** 读完整配置。 */
export function readConfig(env = process.env) {
  // 实例优先；只设 ACB_HARNESS 的老配置把 harness 名当作实例名，保持单实例可用。
  const instance = sanitizeInstance(env.ACB_WB_INSTANCE || env.ACB_HARNESS)
  const harness = env.ACB_HARNESS?.trim() || instance
  if (env.ACB_HARNESS?.trim() && env.ACB_WB_INSTANCE?.trim() && env.ACB_HARNESS.trim() !== env.ACB_WB_INSTANCE.trim()) {
    // 两个实例如果共用 harness id，桥的串行键会撞在一起；直接拒绝。
    throw new Error(t("workbuddy.harness_instance_mismatch", {
      harness: env.ACB_HARNESS.trim(),
      instance: env.ACB_WB_INSTANCE.trim(),
    }))
  }
  const stateDir = resolveStateDir(env)
  const allowedOverride = env.ACB_WB_ALLOWED_TOOLS?.trim()
  return {
    endpoint: env.ACB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: env.ACB_TOKEN?.trim() || "",
    harness,
    instance,
    // 实例的 state 根；子进程的 HOME / CODEBUDDY_CONFIG_DIR / CODEBUDDY_DATA_ROOT
    // 都落在这里，保证登录态和会话记录不跨实例共享。
    stateDir,
    home: stateDir,
    configDir: path.join(stateDir, ".codebuddy"),
    dataRoot: path.join(stateDir, "data"),
    // MCP 每轮由适配器自己起；端口 0 = 系统分配。多实例建议显式给不同端口。
    mcpPort: parsePort(env.ACB_MCP_PORT, 0),
    cliPath: resolveCliPath(env),
    // CLI 的模型由它自己的配置决定；这里只把「预批准」和「设置」透传过去。
    allowedTools: allowedOverride
      ? expandAllowedTools(splitTools(allowedOverride))
      : defaultAllowedTools(),
    settings: env.ACB_WB_SETTINGS?.trim() || '{"enableAllProjectMcpServers":true}',
    // 默认不指定权限模式：--allowedTools 已经预批准桥工具，不需要 bypassPermissions。
    permissionMode: env.ACB_WB_PERMISSION_MODE?.trim() || "",
    maxTurns: env.ACB_WB_MAX_TURNS ? Number(env.ACB_WB_MAX_TURNS) : 0,
    // 可选 --tools 透传。实测 2.147.0 下 --tools "" 并没有移除内置工具，所以默认
    // 不传（null），避免给出「内置工具已关闭」的错觉。
    tools: env.ACB_WB_TOOLS === undefined ? null : env.ACB_WB_TOOLS,
    cwd: env.ACB_WB_CWD?.trim() || process.cwd(),
  }
}
