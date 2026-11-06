// LobsterAI 适配器的配置解析。
//
// 硬约束（来自 docs/plan/harness-adapter-feasibility.md 5.8）：
//   LobsterAI 的模型调用必须走它自己启动的 OpenClaw gateway + 本地 token proxy。
//   裸 OpenClaw（默认 ~/.openclaw、自带 provider）不走 LobsterAI 额度。
//
// 所以这里做三件事：
//   1. 找到 LobsterAI 的 state 目录（<userData>/openclaw/state），找不到就报错；
//   2. 显式拒绝指向 ~/.openclaw 的 state 目录，不做「退化到裸 OpenClaw」的兜底；
//   3. 读 openclaw.json，确认里面确实有 lobsterai-server / 本地 token proxy 的配置。
//
// 网关地址和 token 同样「显式或从 state 目录读」，两样都缺就报错，绝不猜默认端口。

import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { t } from "../../shared/i18n.mjs"

/** LobsterAI 的 userData 目录（Electron app.getPath("userData") 的等价物）。 */
export function lobsterUserDataDir(env = process.env) {
  if (process.platform === "darwin") {
    return path.join(os.homedir(), "Library", "Application Support", "LobsterAI")
  }
  if (process.platform === "win32") {
    const appData = env.APPDATA?.trim() || path.join(os.homedir(), "AppData", "Roaming")
    return path.join(appData, "LobsterAI")
  }
  const configHome = env.XDG_CONFIG_HOME?.trim() || path.join(os.homedir(), ".config")
  return path.join(configHome, "LobsterAI")
}

/** 默认 state 目录：<userData>/openclaw/state；可用 ACB_LA_STATE_DIR / ACB_LA_USER_DATA 覆盖。 */
export function resolveStateDir(env = process.env) {
  const explicit = env.ACB_LA_STATE_DIR?.trim()
  if (explicit) return path.resolve(explicit)
  const userData = env.ACB_LA_USER_DATA?.trim()
  const base = userData ? path.resolve(userData) : lobsterUserDataDir(env)
  return path.join(base, "openclaw", "state")
}

function isDirectory(value) {
  try {
    return fs.statSync(value).isDirectory()
  } catch {
    return false
  }
}

/** 校验 state 目录：存在、且不是裸 OpenClaw 的默认目录。 */
export function assertStateDir(stateDir) {
  const bareOpenClaw = path.resolve(path.join(os.homedir(), ".openclaw"))
  if (path.resolve(stateDir) === bareOpenClaw) {
    throw new Error(t("lobsterai.bare_openclaw", { path: stateDir }))
  }
  if (!isDirectory(stateDir)) {
    throw new Error(t("lobsterai.state_dir_missing", { path: stateDir }))
  }
  return stateDir
}

/** 解析网关地址：显式 ACB_LA_GATEWAY_URL，否则读 gateway-port.json。 */
export function resolveGatewayUrl(stateDir, env = process.env) {
  const explicit = env.ACB_LA_GATEWAY_URL?.trim()
  if (explicit) {
    let parsed
    try {
      parsed = new URL(explicit)
    } catch {
      throw new Error(t("lobsterai.gateway_url_invalid", { url: explicit }))
    }
    if (parsed.protocol !== "ws:") {
      throw new Error(t("lobsterai.gateway_url_invalid", { url: explicit }))
    }
    return parsed.toString()
  }

  const portFile = path.join(stateDir, "gateway-port.json")
  let payload
  try {
    payload = JSON.parse(fs.readFileSync(portFile, "utf8"))
  } catch (error) {
    throw new Error(t("lobsterai.gateway_port_missing", {
      path: portFile,
      error: error?.message ?? String(error),
    }))
  }
  const port = Number(payload?.port)
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(t("lobsterai.gateway_port_missing", {
      path: portFile,
      error: "gateway-port.json has no valid port",
    }))
  }
  return `ws://127.0.0.1:${port}`
}

/** 解析网关 token：显式 ACB_LA_GATEWAY_TOKEN，否则读 state 目录的 gateway-token。 */
export function resolveGatewayToken(stateDir, env = process.env) {
  const explicit = env.ACB_LA_GATEWAY_TOKEN?.trim()
  if (explicit) return explicit
  const tokenFile = path.join(stateDir, "gateway-token")
  let token = ""
  try {
    token = fs.readFileSync(tokenFile, "utf8").trim()
  } catch {
    token = ""
  }
  if (!token) {
    throw new Error(t("lobsterai.gateway_token_missing", { path: tokenFile }))
  }
  return token
}

/**
 * 确认 openclaw.json 确实把模型链路配到了 LobsterAI：
 *   - models.providers 里有 lobsterai-server（或指向 loopback 且带 proxy 标记的 provider），
 *   - 或 plugins.entries.lobster-model-routing 的 token 是 ${LOBSTER_PROXY_TOKEN}。
 *
 * 这一步是「不退化到裸 OpenClaw」的运行期保险：配置不是 LobsterAI 生成的就直接失败，
 * 而不是悄悄用别的 provider 烧自己的钱。
 */
export function assertLobsterQuotaChain(configPath) {
  let config
  try {
    config = JSON.parse(fs.readFileSync(configPath, "utf8"))
  } catch (error) {
    if (!fs.existsSync(configPath)) {
      throw new Error(t("lobsterai.config_missing", { path: configPath }))
    }
    throw new Error(t("lobsterai.config_unreadable", {
      path: configPath,
      error: error?.message ?? String(error),
    }))
  }

  const providers = config?.models?.providers
  const entries = providers && typeof providers === "object" ? Object.entries(providers) : []
  const lobsterProvider = entries.find(([name, provider]) => {
    if (/lobster/i.test(name)) return true
    const baseUrl = typeof provider?.baseUrl === "string" ? provider.baseUrl : ""
    const apiKey = typeof provider?.apiKey === "string" ? provider.apiKey : ""
    return (
      /^http:\/\/(127\.0\.0\.1|localhost)(:\d+)?\//.test(baseUrl) &&
      (apiKey.includes("LOBSTER_PROXY_TOKEN") || apiKey === "proxy-managed")
    )
  })
  const routingPlugin = config?.plugins?.entries?.["lobster-model-routing"]
  const routingToken =
    typeof routingPlugin?.config?.token === "string" ? routingPlugin.config.token : ""
  if (!lobsterProvider && !routingToken.includes("LOBSTER_PROXY_TOKEN")) {
    throw new Error(t("lobsterai.quota_chain_missing", { path: configPath }))
  }
  return {
    provider: lobsterProvider ? lobsterProvider[0] : "lobster-model-routing",
  }
}

/** 读完整配置。任何一项缺失都直接抛错，调用方负责写日志并退出。 */
export function readConfig(env = process.env) {
  const stateDir = assertStateDir(resolveStateDir(env))
  const configPath = path.join(stateDir, "openclaw.json")
  const quota = assertLobsterQuotaChain(configPath)
  const gatewayUrl = resolveGatewayUrl(stateDir, env)
  const gatewayToken = resolveGatewayToken(stateDir, env)

  return {
    endpoint: env.ACB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: env.ACB_TOKEN?.trim() || "",
    harness: env.ACB_HARNESS?.trim() || "lobsterai",
    // MCP 用 streamable-http；生产上给固定端口，openclaw.user.json 里要写同一个 URL。
    mcpPort: env.ACB_MCP_PORT ? Number(env.ACB_MCP_PORT) : 0,
    stateDir,
    configPath,
    quotaProvider: quota.provider,
    gatewayUrl,
    gatewayToken,
    // 可选 agent id。给了就把 session key 写成 agent:<id>:<key>，让 LobsterAI 的
    // 专用 agent 承接 ACB 的轮次（也避免和它自带 IM 通道的 agent 混在一起）。
    agent: env.ACB_LA_AGENT?.trim() || "",
  }
}
