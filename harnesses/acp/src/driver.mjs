// acb 的通用 ACP 适配器：一个进程同时做三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server（streamable-http）：把桥工具暴露成 ACP 会话里的 MCP server。
//      每轮 ACP 的 session/new（或 session/load）都带上这个 URL，agent 自己连。
//   3. 作为 ACP 客户端：按**声明的 argv** 拉起一个 stdio ACP agent，讲 JSON-RPC。
//
// 和旧 WorkBuddy 适配器的区别就一条，但很关键：**启动命令是声明出来的**。
// 适配器不认安装布局、不认 `multi-instance`、不认 headless bundle 的文件名，
// 也不要求 GUI 以某种方式启动。任何能说 ACP 的命令都可以：
//
//   ACB_ACP_COMMAND='["codebuddy-acp","--acp"]'
//   # 或者（可读性更好，能存多个 agent）
//   ACB_ACP_AGENTS='{"codebuddy":{"commands":[{"acp":["codebuddy-acp","--acp"]}]}}'
//   ACB_ACP_AGENT=codebuddy
//
// 工具面、人格、会话映射、后备模型这些桥侧语义和别的适配器一致，不在这里另起一套。

import { BridgeLink, personaSignature, renderPersona, renderTurn, roomText } from "../../opencode/plugin/acb-wire.mjs"
import { bootstrapSurface, residentSurface } from "../../shared/tools.mjs"
import { callBridge, overflowTools, toolDefsForSurface } from "../../shared/bridge-tools.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { t } from "../../shared/i18n.mjs"
import { isMainModule } from "../../shared/main-module.mjs"
import { AcpConnection, AcpError } from "./client.mjs"
import { resolveAgent } from "./agents.mjs"

const DEFAULT_TURN_TIMEOUT_MS = 5 * 60 * 1000
/** ACP 进程按需重启的等待时间；退避避免一个坏命令把日志刷满。 */
const RESTART_BACKOFF_MS = 5_000

/** 每轮追加给 agent 的稳定规则（和 dsh / opencode 同一套话术）。 */
const APPEND_SYSTEM_PROMPT = [
  "[ACB 桥]",
  "你正在通过 AgentChatBridge 与聊天平台对话。桥工具在 MCP 服务器 acb 下，",
  "名字形如 mcp__acb__send_message、mcp__acb__ack_messages。",
  "回本轮房间用 send_message；确认处理完了用 ack_messages 带上投递里的编号。",
  "不要用 shell 或 HTTP 直连桥/平台端口——那样绕过订阅校验和水位，消息不进归档。",
].join("\n")

const log = (level, message) => {
  process.stderr.write(`[acb-acp] ${level} ${message}\n`)
}

/** 从环境读配置。命令行/声明单独解析，这里只管桥侧和进程侧。 */
export function readConfig(env = process.env) {
  return {
    endpoint: env.ACB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: env.ACB_TOKEN?.trim() || "",
    harness: env.ACB_HARNESS?.trim() || "",
    mcpPort: Number(env.ACB_MCP_PORT ?? 0) || 0,
    cwd: env.ACB_ACP_CWD?.trim() || process.cwd(),
    /** allow（默认）= 自动选第一个 allow 选项；reject = 一律拒绝。 */
    permission: (env.ACB_ACP_PERMISSION?.trim() || "allow").toLowerCase(),
    authMethod: env.ACB_ACP_AUTH_METHOD?.trim() || "",
    appendSystemPrompt: env.ACB_ACP_SYSTEM_PROMPT?.trim() || APPEND_SYSTEM_PROMPT,
  }
}

async function main() {
  const config = readConfig()
  if (!config.token) {
    log("error", t("acp.token_missing"))
    process.exit(2)
  }
  if (!config.harness) {
    log("error", t("acp.harness_missing"))
    process.exit(2)
  }

  let agent
  try {
    agent = resolveAgent(process.env)
  } catch (error) {
    log("error", t("acp.agent_config_invalid", { error: error?.message ?? error }))
    process.exit(2)
  }

  const driver = new TurnDriver({ agent, config, log })

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
      log("debug", t("acp.tool_call_end", {
        tool: name,
        ticket: link.ticket ?? "",
        elapsed: Date.now() - started,
      }))
      return result
    } catch (error) {
      log("warn", t("acp.tool_call_failed", {
        tool: name,
        ticket: link.ticket ?? "",
        elapsed: Date.now() - started,
        error: error?.message ?? error,
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
    log("error", t("acp.mcp_listen_failed", {
      port: config.mcpPort || "auto",
      error: error?.message ?? error,
    }))
    process.exit(2)
  }
  driver.mcpUrl = `http://127.0.0.1:${port}/mcp`
  log("info", t("shared.mcp_http_ready", { host: "127.0.0.1", port }))
  log("info", t("acp.driver_ready", {
    endpoint: config.endpoint,
    harness: config.harness,
    agent: agent.id,
    command: agent.acp.join(" "),
    mcpUrl: driver.mcpUrl,
  }))

  void link
    .connect()
    .then(() => {
      driver.seedMaps(link.sessionMaps)
      log("info", t("acp.bridge_connected", { sessions: link.sessions.join(", ") }))
    })
    .catch((error) => log("warn", t("acp.bridge_unreachable", { error: error.message })))

  const shutdown = () => {
    link.close()
    void driver.stop()
    mcpHttp.stop()
    process.exit(0)
  }
  process.on("SIGINT", shutdown)
  process.on("SIGTERM", shutdown)
}

// ---------------------------------------------------------------------------
// 轮次驱动
// ---------------------------------------------------------------------------

export class TurnDriver {
  constructor({ agent, config = {}, log: logger = log, connectionFactory = null }) {
    this.agent = agent
    this.config = config
    this.logger = logger
    this.connectionFactory = connectionFactory ?? ((options) => new AcpConnection(options))
    this.conn = null
    this.connecting = null
    this.mcpUrl = ""
    this.sessionMaps = new Map()
    /** acb 会话 → ACP 会话 id（本进程内已建立/已载入的）。 */
    this.live = new Map()
    /** acb 会话 → 已知的 ACP 会话 id（含桥下发的历史映射）。 */
    this.sessions = new Map()
    /** ACP 会话 id → 已注入的人格签名。 */
    this.personas = new Map()
    /** ACP 会话 id → 已经交代过「怎么用桥」的标记（每条会话只交代一次）。 */
    this.rules = new Set()
    this.chain = Promise.resolve()
    this.surface = bootstrapSurface()
    this.onDemand = []
    this.current = new Map()
    this.lastStartAt = 0
    this.capabilities = {}
  }

  log(level, message) {
    this.logger(level, message)
  }

  toolDefs() {
    return toolDefsForSurface(this.surface, { includeOnDemand: this.onDemand })
  }

  setSurface(surface) {
    if (!surface || !Array.isArray(surface.resident) || !Array.isArray(surface.catalog)) {
      throw new Error("tool surface shape is invalid")
    }
    this.surface = surface
  }

  async #refreshSurface(link, session) {
    try {
      const result = await callBridge(link, "capabilities", { scope: "surface" }, { internal: true })
      const surface = JSON.parse(result)
      this.setSurface(surface)
      this.log("info", t("acp.tool_surface_loaded", {
        session,
        resident: surface.counts?.resident ?? surface.resident.length,
        hidden: surface.counts?.hidden ?? 0,
      }))
    } catch (error) {
      this.surface = residentSurface()
      this.log("warn", t("acp.tool_surface_fallback", { error: error?.message ?? error }))
    }
  }

  seedMaps(maps) {
    if (!Array.isArray(maps)) return
    for (const map of maps) {
      if (!map?.session || !map?.harness_session) continue
      this.sessions.set(map.session, map.harness_session)
      this.sessionMaps.set(map.session, map.harness_session)
      this.log("info", t("acp.session_known", {
        session: map.session,
        sessionId: map.harness_session,
      }))
    }
  }

  /** 保证有一个能用的 ACP 连接；进程退过就按退避重启。 */
  async #connection() {
    if (this.conn && !this.conn.closed) return this.conn
    if (this.connecting) return this.connecting
    const wait = this.lastStartAt + RESTART_BACKOFF_MS - Date.now()
    if (wait > 0) await new Promise((resolve) => setTimeout(resolve, wait))
    this.connecting = (async () => {
      const conn = this.connectionFactory({
        argv: this.agent.acp,
        cwd: this.config.cwd,
        // agent 子进程的环境：默认继承本进程；测试按用例注入模式变量。
        env: this.config.env ?? process.env,
        log: (level, message) => this.log(level, message),
        onNotification: (message) => this.#onNotification(message),
        onRequest: (message) => this.#onRequest(message),
      })
      let init
      try {
        await conn.start()
        init = await conn.initialize({
          clientInfo: { name: "acb-acp", version: "0.1.0" },
          meta: { acb: { harness: this.config.harness ?? "" } },
        })
      } catch (error) {
        // 握手失败也要把子进程收掉，别留一个卡在半路的 agent。
        await conn.stop().catch(() => {})
        throw error
      }
      this.capabilities = conn.capabilities ?? {}
      this.log("info", t("acp.agent_ready", {
        agent: this.agent.id,
        version: String(init?.protocolVersion ?? ""),
        loadSession: this.capabilities.loadSession ? "yes" : "no",
        auth: (conn.authMethods ?? []).map((method) => method.id).join(","),
      }))
      if (this.config.authMethod) {
        await conn.authenticate(this.config.authMethod)
        this.log("info", t("acp.agent_authenticated", { method: this.config.authMethod }))
      }
      this.conn = conn
      this.lastStartAt = Date.now()
      // 新进程里没有旧会话：把 live 清掉，下一轮按 session/load 重新接。
      this.live.clear()
      this.personas.clear()
      this.rules.clear()
      return conn
    })()
    try {
      return await this.connecting
    } finally {
      this.connecting = null
    }
  }

  #mcpServers() {
    return this.mcpUrl
      ? [{ type: "http", name: "acb", url: this.mcpUrl, headers: [] }]
      : []
  }

  /** 这个 acb 会话的 ACP 会话：本进程内复用 > 按历史 id 载入 > 新建。 */
  async #session(acbSession) {
    const live = this.live.get(acbSession)
    if (live) return live
    const conn = await this.#connection()
    const known = this.sessions.get(acbSession)
    if (known && this.capabilities.loadSession) {
      try {
        await conn.sessionLoad({
          sessionId: known,
          cwd: this.config.cwd,
          mcpServers: this.#mcpServers(),
        })
        this.live.set(acbSession, known)
        this.log("info", t("acp.session_loaded", { session: acbSession, sessionId: known }))
        return known
      } catch (error) {
        this.log("warn", t("acp.session_load_failed", {
          session: acbSession,
          sessionId: known,
          error: error?.message ?? error,
        }))
      }
    }
    const created = await conn.sessionNew({
      cwd: this.config.cwd,
      mcpServers: this.#mcpServers(),
    })
    this.live.set(acbSession, created.sessionId)
    this.sessions.set(acbSession, created.sessionId)
    this.log("info", t("acp.session_created", {
      session: acbSession,
      sessionId: created.sessionId,
    }))
    return created.sessionId
  }

  resetSession(acbSession) {
    const sessionId = this.live.get(acbSession) ?? this.sessions.get(acbSession)
    this.live.delete(acbSession)
    this.sessions.delete(acbSession)
    this.sessionMaps.delete(acbSession)
    if (sessionId) {
      this.personas.delete(sessionId)
      this.rules.delete(sessionId)
    }
    if (sessionId && this.conn && this.conn.supportsClose()) {
      void this.conn.closeSession(sessionId).catch(() => {})
    }
    this.log("info", t("acp.session_reset", { session: acbSession, sessionId: sessionId ?? "-" }))
    return sessionId
  }

  async handleControl(frame, link) {
    const { id, session, command } = frame
    if (command === "reset") {
      try {
        const sessionId = this.resetSession(session)
        link.controlResult({ id, session, command, result: { ok: true, dropped: sessionId ?? null } })
      } catch (error) {
        this.log("error", t("acp.reset_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    if (command === "interrupt") {
      const sessionId = this.live.get(session)
      if (!sessionId) {
        link.controlResult({ id, session, command, error: t("acp.no_session", { session }) })
        return
      }
      try {
        this.conn?.cancel(sessionId)
        link.controlResult({ id, session, command, result: { ok: true, acp_session: sessionId } })
      } catch (error) {
        this.log("error", t("acp.interrupt_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    this.log("warn", t("acp.unknown_command", { command }))
    link.controlResult({ id, session, command, error: t("acp.unknown_command", { command }) })
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
      if (!this.mcpUrl) throw new Error(t("acp.mcp_url_missing"))
      this.onDemand = overflowTools(delivery)
      await this.#refreshSurface(link, delivery.session)
      const sessionId = await this.#session(delivery.session)
      this.current.set(delivery.session, sessionId)
      const text = this.#composePrompt(delivery, sessionId)
      const timeoutMs = turnTimeout(delivery)
      this.log("info", t("acp.turn_start", {
        ticket: delivery.ticket,
        room: roomText(delivery.room),
        count: delivery.records.length,
        sessionId,
        model: delivery.session_info?.model ?? "",
      }))

      const result = await this.conn.prompt({
        sessionId,
        content: [{ type: "text", text }],
        timeoutMs,
      })
      const stopReason = result?.stopReason ?? "end_turn"
      if (stopReason === "cancelled") {
        failure = t("acp.turn_cancelled")
      } else if (stopReason === "refusal") {
        failure = t("acp.turn_refused")
        failureInfo = { component: "model", class: "refused" }
      } else if (stopReason !== "end_turn" && stopReason !== "max_tokens" && stopReason !== "max_turn_requests") {
        this.log("warn", t("acp.stop_reason_unknown", { reason: String(stopReason) }))
      }

      this.sessions.set(delivery.session, sessionId)
      this.#reportMap(delivery.session, sessionId, link)
      this.log("info", t("acp.turn_complete", {
        ticket: delivery.ticket,
        sessionId,
        stopReason: String(stopReason),
        seconds: ((Date.now() - started) / 1000).toFixed(1),
        calls: link.turnCalls ?? 0,
      }))
      if (delivery.records.length > 0 && (link.turnCalls ?? 0) === 0 && stopReason === "end_turn") {
        this.log("warn", t("acp.no_tool_calls", { ticket: delivery.ticket }))
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      failureInfo = classifyError(error)
      this.log("error", t("acp.turn_failed", { error: failure }))
    } finally {
      this.current.delete(delivery.session)
      this.log("info", t("acp.turn_end_sent", {
        ticket: delivery.ticket,
        session: delivery.session,
        at: new Date().toISOString(),
      }))
      try {
        link.endTurn(delivery.ticket, failure, failureInfo)
      } catch (error) {
        this.log("warn", t("acp.turn_end_failed", { error: error.message }))
      }
    }
  }

  #composePrompt(delivery, sessionId) {
    let text = ""
    if (!this.rules.has(sessionId) && this.config.appendSystemPrompt) {
      text = `${this.config.appendSystemPrompt}\n\n`
      this.rules.add(sessionId)
    }
    text += renderTurn(delivery, { tools: this.toolDefs().map((tool) => tool.name) })
    const personaText = renderPersona(delivery.persona)
    const signature = personaSignature(delivery.persona)
    if (personaText && this.personas.get(sessionId) !== signature) {
      const updated = this.personas.has(sessionId) ? "[人格已更新]\n" : ""
      text = `${updated}${personaText}\n\n${text}`
      this.personas.set(sessionId, signature)
      this.log("info", t("acp.persona_injected", {
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
        meta: { agent: this.agent.id },
      },
    })
  }

  #onNotification(message = {}) {
    const update = message.params?.update ?? {}
    const kind = update.sessionUpdate ?? ""
    if (kind === "agent_message_chunk") {
      const text = update.content?.text ?? ""
      if (text) this.log("debug", t("acp.agent_said", { text: text.slice(0, 200) }))
      return
    }
    if (kind === "tool_call" || kind === "tool_call_update") {
      const title = update.title ?? update.kind ?? ""
      this.log("debug", t("acp.tool_call_event", { kind, title: String(title).slice(0, 120) }))
      return
    }
    if (kind === "session_info_update" || kind === "config_option_update" || kind === "available_commands_update") {
      return
    }
    if (kind) this.log("debug", t("acp.update_event", { kind }))
  }

  /** 对方的请求：权限必须先答；文件/终端我们声明过不支持。 */
  async #onRequest(message) {
    if (message.method === "session/request_permission") {
      return this.#answerPermission(message.params ?? {})
    }
    throw new AcpError(-32601, `method not found: ${message.method}`, { method: message.method })
  }

  #answerPermission(params) {
    const options = Array.isArray(params.options) ? params.options : []
    const policy = this.config.permission ?? "allow"
    const wanted = policy === "reject" ? "reject" : "allow"
    const picked =
      options.find((option) => String(option.kind ?? "").startsWith(wanted)) ??
      options.find((option) => String(option.kind ?? "").startsWith("allow")) ??
      options[0] ??
      null
    const title = params.toolCall?.title ?? params.toolCall?.kind ?? ""
    if (!picked) {
      this.log("warn", t("acp.permission_no_option", { tool: String(title) }))
      return { outcome: { outcome: "cancelled" } }
    }
    this.log("info", t("acp.permission_answered", {
      tool: String(title).slice(0, 120),
      option: String(picked.name ?? picked.optionId ?? ""),
      policy,
    }))
    return { outcome: { outcome: "selected", optionId: picked.optionId } }
  }

  async stop() {
    const conn = this.conn
    this.conn = null
    if (conn) await conn.stop()
  }
}

/** 一轮的超时：桥配了 0 就是不设超时（和 dsh 一致），没配才用兜底值。 */
export function turnTimeout(delivery) {
  if (typeof delivery?.turn_timeout_ms === "number") return Math.max(0, delivery.turn_timeout_ms)
  return DEFAULT_TURN_TIMEOUT_MS
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/** ACP / 进程错误 → 结构化失败分类；业务错误返回 null。 */
export function classifyError(error) {
  const kind = String(error?.kind ?? "").toLowerCase()
  if (kind === "timeout") return { component: "backend", class: "timeout" }
  if (kind === "unreachable") return { component: "backend", class: "unreachable" }
  if (kind === "unsupported_version") return { component: "backend", class: "protocol" }
  return classifyFailureText(`${error?.message ?? error}\n${error?.data ? JSON.stringify(error.data) : ""}`)
}

/** 从错误正文猜模型/限流/鉴权故障；没有明确线索就不污染依赖表。 */
export function classifyFailureText(text) {
  const value = String(text ?? "").toLowerCase()
  if (!value) return null
  if (/rate limit|rate_limit|quota|429|too many requests|额度/.test(value)) {
    return { component: "model", class: "rate_limited" }
  }
  if (
    /401|403|unauthorized|forbidden|invalid api key|credentials|authentication required|not logged in|登录|授权|authenticate/.test(
      value,
    )
  ) {
    return { component: "model", class: "auth_failed" }
  }
  if (/model[^.]*(not found|unknown|not configured)|provider[^.]*(not found|not configured)|no such model/.test(value)) {
    return { component: "model", class: "model_unconfigured" }
  }
  if (/model[^.]*(unavailable|refused)|ECONNREFUSED|content[-_ ]?blocked/i.test(value)) {
    return { component: "model", class: "model_unavailable" }
  }
  return null
}

if (isMainModule(import.meta.url)) {
  main().catch((error) => {
    log("error", t("acp.driver_start_failed", { error: error?.message ?? error }))
    process.exit(1)
  })
}
