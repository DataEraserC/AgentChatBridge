// LobsterAI 内置 OpenClaw gateway 的最小 WS 客户端。
//
// 帧格式（OpenClaw 2026.8.1，docs/gateway/protocol.md）：
//   - 第一帧必须是 connect 请求；gateway 会先发 connect.challenge 事件。
//   - 请求 {type:"req", id, method, params}
//   - 响应 {type:"res", id, ok, payload|error}
//   - 事件 {type:"event", event, payload, seq?}
//
// 只用 node:net 手写的 MiniWebSocket，不碰全局 WebSocket（undici WASM 在受限
// 沙箱里会 OOM，dsh 已经踩过）。
//
// 这个文件只认 OpenClaw gateway，不认 acb 桥，也不认聊天平台，所以能单独测。

import { MiniWebSocket } from "../../shared/mini-ws.mjs"
import { t } from "../../shared/i18n.mjs"

/// OpenClaw 2026.8.1 的协议版本；握手声明只接受 4。
const PROTOCOL_VERSION = 4
const CONNECT_TIMEOUT_MS = 10_000
const CHALLENGE_WAIT_MS = 3_000
const DEFAULT_REQUEST_TIMEOUT_MS = 30_000

/** gateway 返回的结构化错误。`code` 与 OpenClaw 的 error.code 一致。 */
export class GatewayError extends Error {
  constructor(code, message, details = null) {
    super(message)
    this.name = "GatewayError"
    this.code = code
    this.details = details
  }
}

/** 一个到 OpenClaw gateway 的长连接。 */
export class OpenClawGateway {
  constructor({
    url,
    token,
    log = () => {},
    clientId = "acb-lobsterai",
    clientVersion = "0.1.0",
  }) {
    if (!url) throw new Error(t("lobsterai.gateway_url_invalid", { url: "" }))
    if (!token) throw new Error(t("lobsterai.gateway_token_missing", { path: "(not configured)" }))
    this.url = url
    this.token = token
    this.log = log
    this.clientId = clientId
    this.clientVersion = clientVersion

    this.ws = null
    this.hello = null
    this.seq = 0
    this.pending = new Map()
    this.eventHandlers = new Set()
    this.closed = false
    this.connecting = null
    this.connectSent = false
    this.connectResolve = null
    this.connectReject = null
    this.challengeTimer = null
    this.connectTimer = null
  }

  get connected() {
    return Boolean(this.hello) && Boolean(this.ws?.connected)
  }

  /** 连上去并完成 connect / hello-ok 握手。 */
  async connect() {
    if (this.connected) return
    if (this.connecting) return this.connecting

    this.connecting = (async () => {
      this.closed = false
      this.hello = null
      this.connectSent = false
      const ws = new MiniWebSocket(this.url, { timeoutMs: CONNECT_TIMEOUT_MS })
      this.ws = ws
      ws.onMessage = (text) => this.#onMessage(text)
      ws.onError = (error) => {
        this.log("warn", t("lobsterai.gateway_ws_error", { error: error?.message ?? error }))
      }
      ws.onClose = () => this.#onClose()

      await ws.connect()
      await new Promise((resolve, reject) => {
        this.connectResolve = resolve
        this.connectReject = reject
        // gateway 正常会先发 challenge；老版本没有就等 3 秒后自己发 connect。
        this.challengeTimer = setTimeout(() => this.#sendConnect(), CHALLENGE_WAIT_MS)
        this.connectTimer = setTimeout(() => {
          reject(new GatewayError("timeout", t("lobsterai.gateway_connect_timeout")))
        }, CONNECT_TIMEOUT_MS)
      })
      this.log("info", t("lobsterai.gateway_connected", {
        protocol: this.hello?.protocol ?? PROTOCOL_VERSION,
        server: this.hello?.server?.version ?? "unknown",
      }))
    })()

    try {
      await this.connecting
    } finally {
      this.connecting = null
    }
  }

  /** 注册事件回调；返回取消函数。 */
  onEvent(handler) {
    this.eventHandlers.add(handler)
    return () => this.eventHandlers.delete(handler)
  }

  /** 发一个 RPC 并等响应。超时抛 `timeout`，gateway 拒绝抛对应 code。 */
  request(method, params = {}, { timeoutMs = DEFAULT_REQUEST_TIMEOUT_MS } = {}) {
    if (!this.ws?.connected || !this.hello) {
      return Promise.reject(new GatewayError("not-connected", t("lobsterai.gateway_not_connected")))
    }
    const id = `r-${++this.seq}`
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id)
        reject(new GatewayError("timeout", t("lobsterai.gateway_request_timeout", { method })))
      }, timeoutMs)
      this.pending.set(id, { resolve, reject, timer })
      this.ws.send(JSON.stringify({ type: "req", id, method, params }))
    })
  }

  /** 发送一轮用户消息，返回 `{runId, status}`。 */
  async chatSend({ sessionKey, message, idempotencyKey, timeoutMs }) {
    const payload = await this.request(
      "chat.send",
      { sessionKey, message, idempotencyKey },
      { timeoutMs: timeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS },
    )
    if (!payload || typeof payload.runId !== "string" || !payload.runId) {
      throw new GatewayError(
        "invalid-response",
        t("lobsterai.chat_send_no_run", { payload: JSON.stringify(payload ?? null).slice(0, 300) }),
      )
    }
    return payload
  }

  /** 等一轮真正结束（lifecycle end/error）。`status` 只会是 ok / error / timeout / pending。 */
  agentWait(runId, timeoutMs) {
    return this.request(
      "agent.wait",
      { runId, timeoutMs },
      { timeoutMs: timeoutMs + 5_000 },
    )
  }

  /** 订阅某个 session 的 transcript 事件（会话内 tool/assistant 变更）。失败不致命。 */
  subscribeMessages(sessionKey) {
    return this.request("sessions.messages.subscribe", { sessionKey }, { timeoutMs: 15_000 })
  }

  /** 打断某个 run。 */
  abort({ sessionKey, runId }) {
    return this.request(
      "chat.abort",
      { sessionKey, ...(runId ? { runId } : {}) },
      { timeoutMs: 15_000 },
    )
  }

  close() {
    this.closed = true
    if (this.challengeTimer) clearTimeout(this.challengeTimer)
    if (this.connectTimer) clearTimeout(this.connectTimer)
    this.challengeTimer = null
    this.connectTimer = null
    this.#rejectPending(new GatewayError("closed", t("lobsterai.gateway_closed")))
    this.ws?.close()
    this.ws = null
    this.hello = null
    this.connectResolve = null
    this.connectReject = null
  }

  // -------------------------------------------------------------------------
  // 内部
  // -------------------------------------------------------------------------

  #sendConnect() {
    if (this.connectSent || !this.ws?.connected) return
    this.connectSent = true
    if (this.challengeTimer) clearTimeout(this.challengeTimer)
    this.challengeTimer = null

    const id = `connect-${++this.seq}`
    const timer = setTimeout(() => {
      this.pending.delete(id)
      this.#failConnect(new GatewayError("timeout", t("lobsterai.gateway_connect_timeout")))
    }, CONNECT_TIMEOUT_MS)
    this.pending.set(id, {
      resolve: (payload) => this.#helloOk(payload),
      reject: (error) => this.#failConnect(error),
      timer,
    })

    const params = {
      minProtocol: PROTOCOL_VERSION,
      maxProtocol: PROTOCOL_VERSION,
      client: {
        id: this.clientId,
        version: this.clientVersion,
        platform: process.platform,
        mode: "operator",
      },
      role: "operator",
      scopes: ["operator.read", "operator.write"],
      caps: [],
      commands: [],
      permissions: {},
      auth: { token: this.token },
      locale: "en-US",
      userAgent: `acb-lobsterai/${this.clientVersion}`,
    }
    this.ws.send(JSON.stringify({ type: "req", id, method: "connect", params }))
  }

  #helloOk(payload) {
    if (!payload || payload.type !== "hello-ok") {
      this.#failConnect(new GatewayError(
        "invalid-handshake",
        t("lobsterai.gateway_bad_hello", { payload: JSON.stringify(payload ?? null).slice(0, 300) }),
      ))
      return
    }
    this.hello = payload
    if (this.connectTimer) clearTimeout(this.connectTimer)
    this.connectTimer = null
    const resolve = this.connectResolve
    this.connectResolve = null
    this.connectReject = null
    resolve?.()
  }

  #failConnect(error) {
    if (this.connectTimer) clearTimeout(this.connectTimer)
    this.connectTimer = null
    const reject = this.connectReject
    this.connectResolve = null
    this.connectReject = null
    reject?.(error)
  }

  #onMessage(text) {
    let frame
    try {
      frame = JSON.parse(text)
    } catch {
      this.log("warn", t("lobsterai.gateway_non_json", { text: text.slice(0, 200) }))
      return
    }

    if (frame?.type === "event") {
      if (frame.event === "connect.challenge") {
        this.#sendConnect()
      }
      for (const handler of this.eventHandlers) {
        try {
          handler(frame)
        } catch (error) {
          this.log("warn", t("lobsterai.gateway_event_failed", {
            event: frame.event,
            error: error?.message ?? error,
          }))
        }
      }
      return
    }

    if (frame?.type === "res") {
      const waiting = this.pending.get(frame.id)
      if (!waiting) return
      this.pending.delete(frame.id)
      clearTimeout(waiting.timer)
      if (frame.ok) {
        waiting.resolve(frame.payload ?? null)
      } else {
        waiting.reject(new GatewayError(
          frame.error?.code ?? "request-failed",
          frame.error?.message ?? t("lobsterai.gateway_request_failed"),
          frame.error?.details ?? null,
        ))
      }
      return
    }

    if (frame?.type === "req") {
      // gateway 也会向客户端发 req（node.invoke 之类）。ACB 不需要这些能力，
      // 明确回一个 unavailable，别让对面挂着等。
      this.ws?.send(JSON.stringify({
        type: "res",
        id: frame.id,
        ok: false,
        error: { code: "UNAVAILABLE", message: "acb adapter does not implement gateway client requests" },
      }))
    }
  }

  #onClose() {
    this.ws = null
    this.hello = null
    this.closed = true
    this.#rejectPending(new GatewayError("closed", t("lobsterai.gateway_closed")))
    this.#failConnect(new GatewayError("closed", t("lobsterai.gateway_closed")))
  }

  #rejectPending(error) {
    for (const [, waiting] of this.pending) {
      clearTimeout(waiting.timer)
      waiting.reject(error)
    }
    this.pending.clear()
  }
}
