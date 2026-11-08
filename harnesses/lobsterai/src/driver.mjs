// acb 的 LobsterAI 适配器：一个进程同时做三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server（streamable-http）：把桥工具暴露给 LobsterAI 内置的
//      OpenClaw agent。openclaw.user.json 里注册同一个 URL。
//   3. 作为 LobsterAI gateway 的客户端（OpenClawGateway）：chat.send 投话，
//      agent.wait 等这一轮真正结束。
//
// 为什么必须接 LobsterAI 的 gateway：LobsterAI 的模型链路是
// lobsterai-server → 本地 token proxy → lobsterai-server.youdao.com；裸 OpenClaw
// 不会用这条链路，也就不消耗 LobsterAI 额度。配置里的 state 目录和 quota 检查
// 就是为了不让适配器悄悄退化成裸 OpenClaw。
//
// 拓扑：
//   [桥 + 平台] ←acb TCP→ [LobsterAI 适配器（本进程）]
//                            ├─ MCP streamable-http（LobsterAI 的 agent 连它）
//                            └─ WS → LobsterAI 内置 gateway（127.0.0.1）
//
// 配置（环境变量，详见 README/示例）：
//   ACB_ENDPOINT        桥的接入点，默认 127.0.0.1:7700
//   ACB_TOKEN           设备令牌，必填
//   ACB_HARNESS         harness 名，默认 lobsterai
//   ACB_MCP_PORT        MCP 监听端口；生产上给固定值，0 = 系统分配
//   ACB_LA_STATE_DIR    <userData>/openclaw/state；不给就按 userData 推导
//   ACB_LA_USER_DATA    LobsterAI 的 userData 目录（可选）
//   ACB_LA_GATEWAY_URL  显式 gateway ws:// 地址；不给就读 gateway-port.json
//   ACB_LA_GATEWAY_TOKEN 显式 gateway token；不给就读 gateway-token
//   ACB_LA_AGENT       可选 agent id，给 session key 加 agent: 前缀

import { BridgeLink, personaSignature, renderPersona, renderTurn, roomText } from "../../opencode/plugin/acb-wire.mjs"
import { bootstrapSurface, residentSurface } from "../../shared/tools.mjs"
import { callBridge, overflowTools, toolDefsForSurface } from "../../shared/bridge-tools.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { t } from "../../shared/i18n.mjs"
import { readConfig } from "./config.mjs"
import { GatewayError, OpenClawGateway } from "./openclaw-gateway.mjs"

const DEFAULT_TURN_TIMEOUT_MS = 5 * 60 * 1000
const WAIT_CHUNK_MS = 30_000

const log = (level, message) => {
  // stdout 留给 MCP 之外没什么；但保持一致：诊断全走 stderr。
  process.stderr.write(`[acb-lobsterai] ${level} ${message}\n`)
}

/**
 * 模型看到的桥工具在 OpenClaw MCP 里带前缀 `mcp__<server>__<tool>`。
 * renderTurn 里写的是不带前缀的名字；这里补一句，省得每轮先失败一次。
 */
const TOOL_NAME_HINT = [
  "[桥工具] 这些工具通过 MCP 暴露，名字形如 mcp__acb__send_message、mcp__acb__ack_messages。",
  "回本房间用 send_message（带前缀的那个）；确认处理完了用 ack_messages。",
  "不要用 shell 或 HTTP 直连桥/平台端口——那样绕过订阅校验和水位，消息不进归档。",
].join("\n")

async function main() {
  const config = readConfig()
  if (!config.token) {
    log("error", t("lobsterai.token_missing"))
    process.exit(2)
  }

  const gateway = new OpenClawGateway({
    url: config.gatewayUrl,
    token: config.gatewayToken,
    log,
  })
  const driver = new TurnDriver({
    gateway,
    log,
    agent: config.agent,
    stateDir: config.stateDir,
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
      log("debug", t("lobsterai.tool_call_end", {
        tool: name,
        ticket: link.ticket ?? "",
        elapsed: Date.now() - started,
      }))
      return result
    } catch (error) {
      log("warn", t("lobsterai.tool_call_end", {
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
    // mcp-http-server 的 ready 日志带 dsh 前缀；这里自己打一条本适配器的。
    log: () => {},
  })
  const port = await mcpHttp.start({ port: config.mcpPort })
  log("info", t("shared.mcp_http_ready", { host: "127.0.0.1", port }))
  log("info", t("lobsterai.driver_ready", {
    endpoint: config.endpoint,
    gateway: config.gatewayUrl,
    stateDir: config.stateDir,
    provider: config.quotaProvider,
    mcpUrl: `http://127.0.0.1:${port}/mcp`,
  }))

  void link
    .connect()
    .then(() => {
      driver.seedMaps(link.sessionMaps)
      log("info", t("lobsterai.bridge_connected", { sessions: link.sessions.join(", ") }))
    })
    .catch((error) => log("warn", t("lobsterai.bridge_unreachable", { error: error.message })))

  // gateway 由 LobsterAI 启停；现在连不上不算启动失败，开轮时会再连。
  void gateway
    .connect()
    .catch((error) => log("warn", t("lobsterai.gateway_unreachable", { error: error?.message ?? error })))

  const shutdown = () => {
    link.close()
    gateway.close()
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
  constructor({ gateway, log, agent = "", stateDir = "" }) {
    this.gateway = gateway
    this.log = log
    this.agent = agent
    this.stateDir = stateDir
    // acb session id -> OpenClaw session key（acb-<id> 或 agent:<id>:acb-<id>）。
    this.sessions = new Map()
    // reset 过的会话换带代次的新 key，旧会话留在 gateway 里不删。
    this.generations = new Map()
    // session key -> 已注入的人格签名；人格只在首轮/变更时注入。
    this.personas = new Map()
    this.chain = Promise.resolve()
    // MCP 客户端只在连接时同步一次 tools/list，启动面必须带目录层。
    this.surface = bootstrapSurface()
    this.surfaceStatus = "resident_only"
    this.onDemand = []
    // session -> 当前在跑的 {sessionKey, runId}，interrupt 用。
    this.current = new Map()
  }

  /** 当前 MCP tools/list 要返回的工具定义。 */
  toolDefs() {
    return toolDefsForSurface(this.surface, { includeOnDemand: this.onDemand })
  }

  /** 用桥算出的 surface 替换本轮可见面；形状不对就保留上一次。 */
  setSurface(surface) {
    if (!surface || !Array.isArray(surface.resident) || !Array.isArray(surface.catalog)) {
      throw new Error("tool surface shape is invalid")
    }
    this.surface = surface
    this.surfaceStatus = "ok"
  }

  /** 开轮后、投话前向桥要一次 surface。失败退到常驻层，不退回全量。 */
  async #refreshSurface(link, session) {
    try {
      const result = await callBridge(link, "capabilities", { scope: "surface" }, { internal: true })
      const surface = JSON.parse(result)
      this.setSurface(surface)
      this.log("info", t("lobsterai.tool_surface_loaded", {
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
      this.log("warn", t("lobsterai.tool_surface_fallback", { error: error?.message ?? error }))
    }
  }

  /** 把桥落盘的旧映射 seed 进来，进程重启后复用同一个 session key。 */
  seedMaps(maps) {
    if (!Array.isArray(maps)) return
    for (const map of maps) {
      if (!map?.session || !map?.harness_session) continue
      if (this.sessions.has(map.session)) continue
      this.sessions.set(map.session, map.harness_session)
      this.log("info", t("lobsterai.session_reused", {
        session: map.session,
        key: map.harness_session,
      }))
    }
  }

  resetSession(acbSession) {
    this.sessions.delete(acbSession)
    this.personas.delete(acbSession)
    this.generations.set(acbSession, Date.now().toString(36))
    this.log("info", t("lobsterai.session_reset", { session: acbSession }))
  }

  async handleControl(frame, link) {
    const { id, session, command } = frame
    if (command === "reset") {
      try {
        this.resetSession(session)
        link.controlResult({ id, session, command, result: { ok: true } })
      } catch (error) {
        this.log("error", t("lobsterai.reset_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    if (command === "interrupt") {
      const running = this.current.get(session)
      if (!running) {
        link.controlResult({
          id,
          session,
          command,
          error: t("lobsterai.no_mapping", { session }),
        })
        return
      }
      try {
        await this.gateway.abort(running)
        link.controlResult({ id, session, command, result: { ok: true, run_id: running.runId } })
      } catch (error) {
        this.log("error", t("lobsterai.interrupt_failed", { session, error: error?.message ?? error }))
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    this.log("warn", t("lobsterai.unknown_command", { command }))
    link.controlResult({ id, session, command, error: t("lobsterai.unknown_command", { command }) })
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
      const sessionKey = this.#sessionKey(delivery.session, link)
      this.onDemand = overflowTools(delivery)
      await this.#refreshSurface(link, delivery.session)
      const text = this.#composePrompt(delivery, sessionKey)
      const model = delivery.session_info?.model
        ? t("lobsterai.model_default_with", { model: delivery.session_info.model })
        : t("lobsterai.model_default")
      this.log("info", t("lobsterai.turn_start", {
        ticket: delivery.ticket,
        room: roomText(delivery.room),
        count: delivery.records.length,
        key: sessionKey,
        model,
      }))

      await this.#ensureGateway()
      // transcript 订阅是辅助信息；失败不挡这一轮。
      try {
        await this.gateway.subscribeMessages(sessionKey)
      } catch (error) {
        this.log("debug", t("lobsterai.subscribe_failed", { error: error?.message ?? error }))
      }

      const sent = await this.gateway.chatSend({
        sessionKey,
        message: text,
        idempotencyKey: delivery.ticket,
      })
      this.current.set(delivery.session, { sessionKey, runId: sent.runId })
      this.log("info", t("lobsterai.run_started", {
        ticket: delivery.ticket,
        runId: sent.runId,
        status: sent.status ?? "started",
      }))

      const outcome = await this.#waitRun(delivery, sessionKey, sent.runId)
      if (outcome.error) {
        failure = outcome.error
        failureInfo = outcome.failure ?? null
        this.log("error", t("lobsterai.turn_failed", { error: failure }))
      } else {
        this.log("info", t("lobsterai.turn_complete", {
          ticket: delivery.ticket,
          seconds: ((Date.now() - started) / 1000).toFixed(1),
          calls: link.turnCalls ?? 0,
        }))
        if (delivery.records.length > 0 && (link.turnCalls ?? 0) === 0) {
          this.log("warn", t("lobsterai.no_tool_calls", { ticket: delivery.ticket }))
        }
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      failureInfo = classifyError(error)
      this.log("error", t("lobsterai.turn_failed", { error: failure }))
    } finally {
      this.current.delete(delivery.session)
      this.log("info", t("lobsterai.turn_end_sent", {
        ticket: delivery.ticket,
        session: delivery.session,
        at: new Date().toISOString(),
      }))
      try {
        link.endTurn(delivery.ticket, failure, failureInfo)
      } catch (error) {
        this.log("warn", t("lobsterai.turn_end_failed", { error: error.message }))
      }
    }
  }

  /** 这个 acb 会话对应的 OpenClaw session key（重置后带代次）。 */
  #sessionKey(acbSession, link) {
    const cached = this.sessions.get(acbSession)
    if (cached) return cached
    const generation = this.generations.get(acbSession)
    const base = `acb-${acbSession}${generation ? `-r${generation}` : ""}`
    const key = this.agent ? `agent:${this.agent}:${base}` : base
    this.sessions.set(acbSession, key)
    this.log("info", t("lobsterai.session_ready", { key, agent: this.agent || "(default)" }))
    if (link) {
      link.controlResult({
        id: `map-${Date.now()}`,
        session: acbSession,
        command: "report_map",
        result: {
          session_id: key,
          meta: { agent: this.agent || null, state_dir: this.stateDir || null },
        },
      })
    }
    return key
  }

  /**
   * 本轮 prompt = 投递正文 + 首轮/变更时的人格卡 + 一行 MCP 工具名提示。
   * 人格只在签名变化时注入，重复注入会白烧 token。
   */
  #composePrompt(delivery, sessionKey) {
    let text = renderTurn(delivery, { tools: this.toolDefs().map((tool) => tool.name) })
    const personaText = renderPersona(delivery.persona)
    const signature = personaSignature(delivery.persona)
    if (personaText && this.personas.get(sessionKey) !== signature) {
      const updated = this.personas.has(sessionKey) ? "[人格已更新]\n" : ""
      text = `${updated}${personaText}\n\n${text}`
      this.personas.set(sessionKey, signature)
      this.log("info", t("lobsterai.persona_injected", {
        persona: delivery.persona?.id ?? "",
        session: sessionKey,
      }))
    }
    return `${text}\n\n${TOOL_NAME_HINT}`
  }

  /** gateway 由 LobsterAI 启停；开轮时没连上就现在连。 */
  async #ensureGateway() {
    if (this.gateway.connected) return
    await this.gateway.connect()
  }

  /**
   * 等这一轮结束（agent.wait 返回 lifecycle end/error）。
   *
   * 超时语义跟桥的 turn_timeout_ms 走：
   * - 0 = 关闭，一直等；
   * - n > 0 = 到点还没结束就 chat.abort 打断，算这一轮失败。
   *
   * agent.wait 服务端默认最多等 30 秒；这里分片等，避免一次请求挂太久。
   */
  async #waitRun(delivery, sessionKey, runId) {
    const turnTimeout =
      typeof delivery.turn_timeout_ms === "number" ? delivery.turn_timeout_ms : DEFAULT_TURN_TIMEOUT_MS
    const deadline = turnTimeout > 0 ? Date.now() + turnTimeout : null

    for (;;) {
      const chunkMs = deadline === null
        ? WAIT_CHUNK_MS
        : Math.max(1, Math.min(WAIT_CHUNK_MS, deadline - Date.now()))
      let snapshot
      try {
        snapshot = await this.gateway.agentWait(runId, chunkMs)
      } catch (error) {
        // gateway 断开或请求超时：这轮不可能再等到结束信号，明确失败。
        return {
          error: error?.message ?? String(error),
          failure: classifyError(error) ?? { component: "backend", class: "closed" },
        }
      }

      if (snapshot?.status === "ok") return { error: null }
      if (snapshot?.status === "error") {
        const message = snapshot.error ?? "the run ended with an error"
        return { error: message, failure: classifyFailureText(message) }
      }

      // timeout / pending：服务端只是没等到结束，继续等，直到本轮的 deadline。
      if (deadline !== null && Date.now() >= deadline) {
        this.log("warn", t("lobsterai.run_timeout_abort", {
          seconds: Math.round(turnTimeout / 1000),
          session: sessionKey,
          runId,
        }))
        try {
          await this.gateway.abort({ sessionKey, runId })
        } catch (error) {
          this.log("warn", t("lobsterai.abort_undelivered", { error: error?.message ?? error }))
        }
        return {
          error: t("lobsterai.run_timeout", { seconds: Math.round(turnTimeout / 1000) }),
          failure: { component: "backend", class: "timeout" },
        }
      }
    }
  }
}

// ---------------------------------------------------------------------------
// 错误分类
// ---------------------------------------------------------------------------

/** gateway / transport 层错误 → 结构化失败分类；业务错误返回 null。 */
export function classifyError(error) {
  const code = String(error?.code ?? "").toLowerCase()
  if (code === "closed" || code === "not-connected") {
    return { component: "backend", class: "closed" }
  }
  if (code === "timeout") {
    return { component: "backend", class: "unreachable" }
  }
  if (code === "pairing_required" || code === "missing_scope" || code === "forbidden" || code === "unauthorized") {
    return { component: "backend", class: "auth_failed" }
  }
  return classifyFailureText(error?.message ?? error)
}

/** 从错误正文猜模型/限流/鉴权故障；没有明确线索就不污染依赖表。 */
export function classifyFailureText(text) {
  const value = String(text ?? "").toLowerCase()
  if (!value) return null
  if (/rate limit|rate_limit|quota|429|too many requests|额度/.test(value)) {
    return { component: "model", class: "rate_limited" }
  }
  if (/401|403|unauthorized|forbidden|invalid api key|api key|credentials|登录|授权/.test(value)) {
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
    log("error", t("lobsterai.driver_start_failed", { error: error?.stack ?? error }))
    process.exit(1)
  })
}

export { TurnDriver, GatewayError }
