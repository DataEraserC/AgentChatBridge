// WorkBuddy 适配器的配置解析。
//
// 硬约束（来自 docs/plan/harness-adapter-feasibility.md 4.8）：
//   上游 WorkBuddy 发行物只带 cli/dist/codebuddy-headless.js，没有 dist/codebuddy.js；
//   bin/codebuddy 在非无头命令上会 MODULE_NOT_FOUND。所以适配器只认 headless
//   bundle，或者显式设置 CODEBUDDY_FORCE_HEADLESS_BUNDLE=1 后再用 launcher。
//
// 配置只做三件事：找 CLI、算会话 id 规则、准备每轮 CLI 参数。不读 GUI 的
// workbuddy.db（那是私有面），也不碰 ~/.workbuddy 的内部实现。

import fs from "node:fs"
import path from "node:path"
import { TOOLS } from "../../shared/tools.mjs"
import { t } from "../../shared/i18n.mjs"

const HEADLESS_BUNDLE = "codebuddy-headless.js"

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

/**
 * ACB 会话 id → WorkBuddy 的 --session-id。
 * CLI 允许字母/数字/连字符/下划线/冒号，但这里不用冒号：ACB 房间串里本来就有
 * 冒号，统一换成连字符更干净。重置后加代次后缀，避免和旧会话撞 id。
 */
export function sessionIdFor(acbSession, generation = "") {
  let raw = `acb-${String(acbSession ?? "")}`.replace(/[^A-Za-z0-9_-]/g, "-")
  raw = raw.replace(/-+/g, "-").replace(/^[^A-Za-z0-9]+/, "")
  if (!raw) raw = "acb"
  raw = raw.slice(0, 120)
  return generation ? `${raw}-r${generation}` : raw
}

function splitTools(value) {
  return String(value ?? "")
    .split(/[,\s]+/)
    .map((item) => item.trim())
    .filter(Boolean)
}

/** 默认预批准全部桥工具：裸名和 MCP 前缀名都给，避免 CLI 在非交互模式下卡权限。 */
function defaultAllowedTools() {
  return TOOLS.flatMap((tool) => [tool.name, `mcp__acb__${tool.name}`])
}

/** 读完整配置。 */
export function readConfig(env = process.env) {
  const allowedOverride = env.ACB_WB_ALLOWED_TOOLS?.trim()
  return {
    endpoint: env.ACB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: env.ACB_TOKEN?.trim() || "",
    harness: env.ACB_HARNESS?.trim() || "workbuddy",
    // MCP 每轮由适配器自己起；端口 0 = 系统分配，CLI 通过 --mcp-config 拿 URL。
    mcpPort: env.ACB_MCP_PORT ? Number(env.ACB_MCP_PORT) : 0,
    cliPath: resolveCliPath(env),
    // CLI 的模型由它自己的配置决定；这里只把「预批准」和「设置」透传过去。
    allowedTools: allowedOverride ? splitTools(allowedOverride) : defaultAllowedTools(),
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
