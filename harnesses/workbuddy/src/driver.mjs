// acb 的 WorkBuddy 适配器：一个进程同时做三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server（streamable-http）：把桥工具暴露给 WorkBuddy CLI。
//      每轮 CLI 用 --mcp-config 连这个本地 URL，--strict-mcp-config 保证只连它。
//   3. 作为 WorkBuddy CLI 的驱动：每轮 spawn
//      `cli/dist/codebuddy-headless.js -p ... --output-format stream-json`，
//      进程退出（或最终 result 事件）就是这一轮结束。
//
// 为什么用 headless bundle：上游 WorkBuddy 发行物没有 dist/codebuddy.js，
// bin/codebuddy 的 full TUI 入口会 MODULE_NOT_FOUND（见 feasibility 4.8）。
// 适配器只认 codebuddy-headless.js；如果用户显式给了 launcher，必须设置
// CODEBUDDY_FORCE_HEADLESS_BUNDLE=1，否则配置阶段就拒绝。
//
// 配置（环境变量，详见 workbuddy.env.example）：
//   ACB_ENDPOINT      桥的接入点，默认 127.0.0.1:7700
//   ACB_TOKEN         设备令牌，必填
//   ACB_HARNESS       harness 名，默认 workbuddy
//   ACB_MCP_PORT      MCP 监听端口；0 = 系统分配（每轮 URL 会传给 CLI）
//   ACB_WB_CLI        codebuddy-headless.js 的路径；不给就从 workbuddy 安装布局推导
//   ACB_WB_SETTINGS   --settings 的 JSON 字符串
//   ACB_WB_PERMISSION_MODE  --permission-mode（默认不设）
//   ACB_WB_MAX_TURNS  --max-turns（默认不设）
//   ACB_WB_TOOLS      --tools（默认 ""，只留 MCP 工具）
//   ACB_WB_CWD        CLI 的工作目录，默认当前目录

import { BridgeLink, personaSignature, renderPersona, renderTurn, roomText } from "../../opencode/plugin/acb-wire.mjs"
import { residentSurface } from "../../shared/tools.mjs"
import { callBridge, overflowTools, toolDefsForSurface } from "../../shared/bridge-tools.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { t } from "../../shared/i18n.mjs"
import { CodebuddyClient, isResumeMissing } from "./codebuddy-client.mjs"
import { readConfig, sessionIdFor } from "./config.mjs"

const DEFAULT_TURN_TIMEOUT_MS = 5 * 60 * 1000

const log = (level, message) => {
  process.stderr.write(`[acb-workbuddy] ${level} ${message}\n`)
}

/**
 * 每轮追加到 CLI system prompt 的稳定规则。工具名是 WorkBuddy 的 MCP 命名
 * （`mcp__<server>__<tool>`）；ACB 的 server 名固定为 acb。
 */
const APPEND_SYSTEM_PROMPT = [
  "[ACB 桥]",
  "你正在通过 AgentChatBridge 与聊天平台对话。桥工具在 MCP 服务器 acb 下，",
  "名字形如 mcp__acb__send_message、mcp__acb__ack_messages。",
  "回本轮房间用 send_message；确认处理完了用 ack_messages 带上投递里的编号。",
  "不要用 shell 或 HTTP 直连桥/平台端口——那样绕过订阅校验和水位，消息不进归档。",
].join("\n")

async function main() {
  const config = readConfig()
  if (!config.token) {
    log("error", t("workbuddy.token_missing"))
    process.exit(2)
  }

  const cli = new CodebuddyClient({
    cliPath: config.cliPath,
    log,
    cwd: config.cwd,
    home: config.home,
    configDir: config.configDir,
    dataRoot: config.dataRoot,
  })
  const driver = new TurnDriver({
    cli,
    log,
    instance: config.instance,
    allowedTools: config.allowedTools,
    settings: config.settings,
    permissionMode: config.permissionMode,
    maxTurns: config.maxTurns,
    tools: config.tools,
    appendSystemPrompt: APPEND_SYSTEM_PROMPT,
  })

  const link = new BridgeLink({
    endpoint: config.endpoint,
    token: config.token,
    harness: config.harness,
    log,
    onTurn: (delivery) => driver.run(delivery, link),
    onControl: (frame) => driver.handleControl(frame, link),
  })

  const onCall = async (name, args) => {
    const started = Date.now()
    try {
      const result = await callBridge(link, name, args)
      log("debug", t("workbuddy.tool_call_end", {
        tool: name,
        ticket: link.ticket ?? "",
        elapsed: Date.now() - started,
      }))
      return result
    } catch (error) {
      log("warn", t("workbuddy.tool_call_end", {
        tool: name,
        ticket: link.ticket ?? "",
        elapsed: Date.now() - started,
        error: `: ${error?.message ?? error}`,
      }))
      throw error
    }
  }

  const mcpHttp = new McpHttpServer({
    name: "acb",
    version: "0.1.0",
    tools: () => driver.toolDefs(),
    onCall,
    log: () => {},
  })
  let port
  try {
    port = await mcpHttp.start({ port: config.mcpPort })
  } catch (error) {
    log("error", t("workbuddy.mcp_listen_failed", {
      port: config.mcpPort || "auto",
      error: error?.message ?? error,
    }))
    process.exit(2)
  }
  driver.mcpUrl = `http://127.0.0.1:${port}/mcp`
  log("info", t("shared.mcp_http_ready", { host: "127.0.0.1", port }))
  log("info", t("workbuddy.driver_ready", {
    endpoint: config.endpoint,
    instance: config.instance,
    harness: config.harness,
    stateDir: config.stateDir,
    cli: config.cliPath,
    mcpUrl: driver.mcpUrl,
  }))

  void link
    .connect()
    .then(() => {
      driver.seedMaps(link.sessionMaps)
      log("info", t("workbuddy.bridge_connected", { sessions: link.sessions.join(", ") }))
    })
    .catch((error) => log("warn", t("workbuddy.bridge_unreachable", { error: error.message })))

  const shutdown = () => {
    link.close()
    cli.abort()
    mcpHttp.stop()
    process.exit(0)
  }
  process.on("SIGINT", shutdown)
  process.on("SIGTERM", shutdown)
}

// ---------------------------------------------------------------------------
// 轮次驱动
// ---------------------------------------------------------------------------

class TurnDriver {
  constructor({
    cli,
    log,
    instance = "workbuddy-1",
    allowedTools = [],
    settings = "",
    permissionMode = "",
    maxTurns = 0,
    tools = null,
    appendSystemPrompt = "",
  }) {
    this.cli = cli
    this.log = log
    this.instance = instance
    this.allowedTools = allowedTools
    this.settings = settings
    this.permissionMode = permissionMode
    this.maxTurns = maxTurns
    this.tools = tools
    this.appendSystemPrompt = appendSystemPrompt
    this.mcpUrl = ""
    // acb session id -> 已成功建立过的 CLI session id。只有跑成功过才记，
    // 失败的下次仍用 --session-id 新建，避免 resume 一个半成品会话。
    this.sessions = new Map()
    this.generations = new Map()
    // CLI session id -> 已注入的人格签名。
    this.personas = new Map()
    this.chain = Promise.resolve()
    this.surface = residentSurface()
    this.surfaceStatus = "resident_only"
    this.onDemand = []
    // acb session id -> 当前在跑的轮次（interrupt 用）。
    this.current = new Map()
  }

  toolDefs() {
    return toolDefsForSurface(this.surface, { includeOnDemand: this.onDemand })
  }

  setSurface(surface) {
    if (!surface || !Array.isArray(surface.resident) || !Array.isArray(surface.catalog)) {
      throw new Error("tool surface shape is invalid")
    }
    this.surface = surface
    this.surfaceStatus = "ok"
  }

  async #refreshSurface(link, session) {
    try {
      const result = await callBridge(link, "capabilities", { scope: "surface" }, { internal: true })
      const surface = JSON.parse(result)
      this.setSurface(surface)
      this.log("info", t("workbuddy.tool_surface_loaded", {
        session,
        resident: surface.counts?.resident ?? surface.resident.length,
        visible:
          surface.counts?.visible ??
          surface.resident.length + surface.catalog.filter((entry) => entry.direct_executable !== false).length,
        hidden:
          surface.counts?.hidden ??
          surface.catalog.filter((entry) => entry.direct_executable === false).length,
      }))
    } catch (error) {
      this.surface = residentSurface()
      this.surfaceStatus = "surface_fallback"
      this.log("warn", t("workbuddy.tool_surface_fallback", { error: error?.message ?? error }))
    }
  }

  seedMaps(maps) {
    if (!Array.isArray(maps)) return
    for (const map of maps) {
      if (!map?.session || !map?.harness_session) continue
      if (this.sessions.has(map.session)) continue
      this.sessions.set(map.session, map.harness_session)
      this.log("info", t("workbuddy.session_reused", {
        session: map.session,
        sessionId: map.harness_session,
      }))
    }
  }

  resetSession(acbSession) {
    const old = this.sessions.get(acbSession)
    if (old) this.personas.delete(old)
    this.sessions.delete(acbSession)
    this.generations.set(acbSession, Date.now().toString(36))
    this.log("info", t("workbuddy.session_reset", { session: acbSession }))
  }

  async handleControl(frame, link) {
    const { id, session, command } = frame
    if (command === "reset") {
      try {
        this.resetSession(session)
        link.controlResult({ id, session, command, result: { ok: true } })
      } catch (error) {
        this.log("error", t("workbuddy.reset_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    if (command === "interrupt") {
      if (!this.current.has(session)) {
        link.controlResult({
          id,
          session,
          command,
          error: t("workbuddy.no_mapping", { session }),
        })
        return
      }
      try {
        this.cli.abort()
        link.controlResult({ id, session, command, result: { ok: true } })
      } catch (error) {
        this.log("error", t("workbuddy.interrupt_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    this.log("warn", t("workbuddy.unknown_command", { command }))
    link.controlResult({ id, session, command, error: t("workbuddy.unknown_command", { command }) })
  }

  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    let failureInfo = null
    const started = Date.now()
    try {
      if (!this.mcpUrl) throw new Error(t("workbuddy.mcp_url_missing"))
      const session = this.#session(delivery.session)
      this.onDemand = overflowTools(delivery)
      await this.#refreshSurface(link, delivery.session)
      const text = this.#composePrompt(delivery, session.id)
      const timeoutMs =
        typeof delivery.turn_timeout_ms === "number" ? delivery.turn_timeout_ms : DEFAULT_TURN_TIMEOUT_MS
      const model = delivery.session_info?.model
        ? t("workbuddy.model_from_session", { model: delivery.session_info.model })
        : t("workbuddy.model_default")
      this.log("info", t("workbuddy.turn_start", {
        ticket: delivery.ticket,
        room: roomText(delivery.room),
        count: delivery.records.length,
        sessionId: session.id,
        resume: session.resume ? "yes" : "no",
        model,
      }))

      let result
      try {
        result = await this.#runCli({ text, session, timeoutMs, acbSession: delivery.session })
      } catch (error) {
        // 旧会话不存在：丢掉映射，用 --session-id 新建同一个 id 重试一次；
        // 还不行就换带代次的新 id，避免死在一个坏 id 上。
        if (!isResumeMissing(error) || !session.resume) throw error
        this.log("warn", t("workbuddy.resume_missing", { sessionId: session.id, error: error?.message ?? error }))
        this.sessions.delete(delivery.session)
        this.personas.delete(session.id)
        try {
          result = await this.#runCli({
            text,
            session: { id: session.id, resume: false },
            timeoutMs,
            acbSession: delivery.session,
          })
        } catch (retryError) {
          if (!isResumeMissing(retryError)) throw retryError
          this.generations.set(delivery.session, Date.now().toString(36))
          const fresh = this.#session(delivery.session)
          this.log("warn", t("workbuddy.session_conflict", { session: delivery.session, sessionId: fresh.id }))
          result = await this.#runCli({
            text,
            session: fresh,
            timeoutMs,
            acbSession: delivery.session,
          })
        }
      }

      this.current.delete(delivery.session)
      this.sessions.set(delivery.session, result.sessionId || session.id)
      this.#reportMap(delivery.session, result.sessionId || session.id, link)
      this.log("info", t("workbuddy.turn_complete", {
        ticket: delivery.ticket,
        sessionId: result.sessionId || session.id,
        seconds: ((Date.now() - started) / 1000).toFixed(1),
        calls: link.turnCalls ?? 0,
      }))
      if (delivery.records.length > 0 && (link.turnCalls ?? 0) === 0) {
        this.log("warn", t("workbuddy.no_tool_calls", { ticket: delivery.ticket }))
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      failureInfo = classifyError(error)
      this.log("error", t("workbuddy.turn_failed", { error: failure }))
    } finally {
      this.current.delete(delivery.session)
      this.log("info", t("workbuddy.turn_end_sent", {
        ticket: delivery.ticket,
        session: delivery.session,
        at: new Date().toISOString(),
      }))
      try {
        link.endTurn(delivery.ticket, failure, failureInfo)
      } catch (error) {
        this.log("warn", t("workbuddy.turn_end_failed", { error: error.message }))
      }
    }
  }

  /** 跑一次 CLI，并把「当前轮」记下来供 interrupt 用。 */
  async #runCli({ text, session, timeoutMs, acbSession = "" }) {
    this.current.set(acbSession, { sessionId: session.id })
    return this.cli.runTurn({
      prompt: text,
      sessionId: session.id,
      resume: session.resume,
      mcpUrl: this.mcpUrl,
      appendSystemPrompt: this.appendSystemPrompt,
      allowedTools: this.allowedTools ?? [],
      settings: this.settings,
      permissionMode: this.permissionMode,
      maxTurns: this.maxTurns,
      tools: this.tools,
      timeoutMs,
    })
  }

  /** 这个 acb 会话这一轮要用的 CLI 会话 id。 */
  #session(acbSession) {
    const created = this.sessions.get(acbSession)
    if (created) return { id: created, resume: true }
    const generation = this.generations.get(acbSession)
    return { id: sessionIdFor(acbSession, this.instance, generation), resume: false }
  }

  #composePrompt(delivery, sessionId) {
    let text = renderTurn(delivery)
    const personaText = renderPersona(delivery.persona)
    const signature = personaSignature(delivery.persona)
    if (personaText && this.personas.get(sessionId) !== signature) {
      const updated = this.personas.has(sessionId) ? "[人格已更新]\n" : ""
      text = `${updated}${personaText}\n\n${text}`
      this.personas.set(sessionId, signature)
      this.log("info", t("workbuddy.persona_injected", {
        persona: delivery.persona?.id ?? "",
        sessionId,
      }))
    }
    return text
  }

  #reportMap(session, sessionId, link) {
    if (!link) return
    link.controlResult({
      id: `map-${Date.now()}`,
      session,
      command: "report_map",
      result: {
        session_id: sessionId,
        meta: { cli: "codebuddy-headless", instance: this.instance },
      },
    })
  }
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/** CLI/模型错误 → 结构化失败分类；业务错误返回 null。 */
export function classifyError(error) {
  const kind = String(error?.kind ?? "").toLowerCase()
  if (kind === "timeout") return { component: "backend", class: "timeout" }
  if (kind === "spawn_failed") return { component: "backend", class: "unreachable" }
  if (kind === "mcp_unavailable") return { component: "backend", class: "mcp_unavailable" }
  if (kind === "aborted") return null
  return classifyFailureText(`${error?.message ?? error}\n${error?.stderr ?? ""}`)
}

/** 从错误正文猜模型/限流/鉴权故障；没有明确线索就不污染依赖表。 */
export function classifyFailureText(text) {
  const value = String(text ?? "").toLowerCase()
  if (!value) return null
  if (/rate limit|rate_limit|quota|429|too many requests|额度/.test(value)) {
    return { component: "model", class: "rate_limited" }
  }
  if (
    /401|403|unauthorized|forbidden|invalid api key|api key|credentials|authentication required|please use \/login|not logged in|登录|授权/.test(
      value,
    )
  ) {
    return { component: "model", class: "auth_failed" }
  }
  if (/model[^.]*(not found|unknown|not configured)|provider[^.]*(not found|not configured)|no such model/.test(value)) {
    return { component: "model", class: "model_unconfigured" }
  }
  if (/model[^.]*(unavailable|refused)|ECONNREFUSED/i.test(value)) {
    return { component: "model", class: "model_unavailable" }
  }
  return null
}

// 作为主模块运行时直接跑；被测试 import 时不动。
// 用 realpath 比较，兼容 profile/current 软链启动（argv[1] 是软链路径）。
import { isMainModule } from "../../shared/main-module.mjs"

const isMain = isMainModule(import.meta.url)
if (isMain) {
  main().catch((error) => {
    log("error", t("workbuddy.driver_start_failed", { error: error?.stack ?? error }))
    process.exit(1)
  })
}

export { TurnDriver, CodebuddyClient }
