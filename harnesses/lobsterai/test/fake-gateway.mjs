// 测试用的假 OpenClaw gateway（WebSocket + JSON 帧）。
//
// 它只实现 acb 适配器用到的那几条：connect 握手、chat.send、agent.wait、
// sessions.messages.subscribe、chat.abort。第二轮起还会扮演「模型」：在
// chat.send 之后调适配器的 MCP server，把一次工具调用真的走完。
//
// 这样测试能覆盖：
//   - turn_start → chat.send → MCP 工具调用 → agent.wait ok → turn_end
//   - 票据：MCP 工具调用会变成带当前 ticket 的 tool_call
//   - 错误路径：chat.send 拒绝、run error、gateway 中途断开
//
// 不引第三方 ws 库：服务端握手和帧解析都手写，和 shared/mini-ws.mjs 对称。

import http from "node:http"
import { websocketAccept } from "../../shared/mini-ws.mjs"

const FRAME_CLOSE = 0x8
const FRAME_TEXT = 0x1
const FRAME_PING = 0x9

// 真 gateway 的注册表（2026-11-13 从 gateway-bundle.mjs 里读出来）。
// 假 gateway 也照这个校验，否则客户端发错 id/mode 也能「通过」测试。
const CLIENT_IDS = new Set([
  "webchat-ui",
  "openclaw-control-ui",
  "openclaw-browser-copilot",
  "openclaw-tui",
  "webchat",
  "cli",
  "gateway-client",
  "openclaw-macos",
  "openclaw-linux",
  "openclaw-ios",
  "openclaw-watchos",
  "openclaw-android",
  "node-host",
  "openclaw-worker",
  "test",
  "fingerprint",
  "openclaw-probe",
])
const CLIENT_MODES = new Set(["webchat", "cli", "ui", "backend", "node", "worker", "probe", "test"])

export class FakeGateway {
  constructor({
    token = "gw-token",
    toolCall = null,
    runStatus = "ok",
    runError = "run failed",
    chatError = null,
    disconnectAfterChatSend = false,
    mcpUrl = "",
  } = {}) {
    this.token = token
    this.toolCall = toolCall
    this.runStatus = runStatus
    this.runError = runError
    this.chatError = chatError
    this.disconnectAfterChatSend = disconnectAfterChatSend
    this.mcpUrl = mcpUrl

    this.server = null
    this.port = 0
    this.socket = null
    this.buffer = Buffer.alloc(0)
    this.requests = []
    this.events = []

    this.runId = "run-1"
    this.runTerminal = null
    this.waiters = []
    this.chatSendCalls = []
    this.subscriptions = []
    this.aborts = []
    this.mcpCalls = []

    this.chatSendWaiters = []
    this.toolDoneWaiters = []
    this.runDoneWaiters = []
    this.toolDone = false
    this.closed = false
  }

  async start() {
    this.server = http.createServer()
    this.server.on("upgrade", (req, socket) => this.#upgrade(req, socket))
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve))
    this.port = this.server.address().port
    return this
  }

  get url() {
    return `ws://127.0.0.1:${this.port}`
  }

  async stop() {
    this.socket?.destroy()
    this.socket = null
    if (!this.server) return
    await new Promise((resolve) => this.server.close(resolve))
    this.server = null
  }

  waitForChatSend() {
    return this.#wait(this.chatSendWaiters, 5_000, "chat.send", () => this.chatSendCalls.length > 0)
  }

  waitForToolDone() {
    return this.#wait(this.toolDoneWaiters, 5_000, "MCP 工具调用", () => this.toolDone)
  }

  waitForRunDone() {
    return this.#wait(this.runDoneWaiters, 5_000, "run 结束", () => Boolean(this.runTerminal))
  }

  /** 手动结束一个 run（不走 toolCall 的测试用）。 */
  completeRun(status = "ok", error = "") {
    this.#settleRun(status, error)
  }

  // -------------------------------------------------------------------------
  // WebSocket 服务端
  // -------------------------------------------------------------------------

  #upgrade(req, socket) {
    const key = req.headers["sec-websocket-key"]
    if (!key) {
      socket.destroy()
      return
    }
    const accept = websocketAccept(key)
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\n" +
        "Upgrade: websocket\r\n" +
        "Connection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    )
    this.socket = socket
    this.buffer = Buffer.alloc(0)
    socket.on("data", (chunk) => this.#onData(chunk))
    socket.on("close", () => {
      this.closed = true
    })
    socket.on("error", () => {})
    // gateway 正常会先发 challenge；适配器收到后才发 connect。
    this.#sendEvent("connect.challenge", { nonce: "n-1", ts: Date.now() })
  }

  #onData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk])
    for (;;) {
      if (this.buffer.length < 2) return
      const b0 = this.buffer[0]
      const b1 = this.buffer[1]
      const opcode = b0 & 0x0f
      const masked = (b1 & 0x80) !== 0
      let length = b1 & 0x7f
      let offset = 2
      if (length === 126) {
        if (this.buffer.length < 4) return
        length = this.buffer.readUInt16BE(2)
        offset = 4
      } else if (length === 127) {
        if (this.buffer.length < 10) return
        length = Number(this.buffer.readBigUInt64BE(2))
        offset = 10
      }
      const mask = masked ? this.buffer.subarray(offset, offset + 4) : null
      if (masked) offset += 4
      if (this.buffer.length < offset + length) return
      const raw = this.buffer.subarray(offset, offset + length)
      this.buffer = this.buffer.subarray(offset + length)

      if (opcode === FRAME_CLOSE) {
        this.socket?.end()
        this.socket = null
        return
      }
      if (opcode === FRAME_PING) {
        this.socket?.write(Buffer.from([0x8a, raw.length]))
        continue
      }
      if (opcode !== FRAME_TEXT) continue
      let payload = raw
      if (mask) {
        payload = Buffer.alloc(raw.length)
        for (let i = 0; i < raw.length; i += 1) payload[i] = raw[i] ^ mask[i % 4]
      }
      let frame
      try {
        frame = JSON.parse(payload.toString("utf8"))
      } catch {
        continue
      }
      this.requests.push(frame)
      this.#onFrame(frame)
    }
  }

  #send(frame) {
    if (!this.socket) return
    const payload = Buffer.from(JSON.stringify(frame), "utf8")
    let header
    if (payload.length < 126) {
      header = Buffer.from([0x81, payload.length])
    } else if (payload.length < 65536) {
      header = Buffer.alloc(4)
      header[0] = 0x81
      header[1] = 126
      header.writeUInt16BE(payload.length, 2)
    } else {
      header = Buffer.alloc(10)
      header[0] = 0x81
      header[1] = 127
      header.writeBigUInt64BE(BigInt(payload.length), 2)
    }
    this.socket.write(Buffer.concat([header, payload]))
  }

  #sendEvent(event, payload) {
    this.events.push({ event, payload })
    this.#send({ type: "event", event, payload })
  }

  #respond(id, payload) {
    this.#send({ type: "res", id, ok: true, payload })
  }

  #respondError(id, code, message, details = null) {
    this.#send({
      type: "res",
      id,
      ok: false,
      error: { code, message, ...(details ? { details } : {}) },
    })
  }

  // -------------------------------------------------------------------------
  // 协议
  // -------------------------------------------------------------------------

  #onFrame(frame) {
    if (frame.type === "req") {
      this.#onRequest(frame)
      return
    }
    if (frame.type === "res") {
      // 适配器回给 gateway 的响应（我们没发过 req，忽略）。
      return
    }
  }

  #onRequest(frame) {
    switch (frame.method) {
      case "connect":
        if (frame.params?.auth?.token !== this.token) {
          this.#respondError(frame.id, "FORBIDDEN", "invalid gateway token")
          return
        }
        if (!CLIENT_IDS.has(frame.params?.client?.id)) {
          this.#respondError(frame.id, "INVALID_REQUEST", "invalid connect params: /client/id")
          return
        }
        if (!CLIENT_MODES.has(frame.params?.client?.mode)) {
          this.#respondError(frame.id, "INVALID_REQUEST", "invalid connect params: /client/mode")
          return
        }
        this.#respond(frame.id, {
          type: "hello-ok",
          protocol: 4,
          server: { version: "2026.8.1", connId: "fake-gateway" },
          features: { methods: ["chat.send", "agent.wait"], events: [] },
          snapshot: {},
          auth: { role: "operator", scopes: ["operator.read", "operator.write"] },
          policy: {
            maxPayload: 26214400,
            maxBufferedBytes: 52428800,
            tickIntervalMs: 15000,
          },
        })
        return
      case "sessions.messages.subscribe":
        // 真 gateway 只认 key；发 sessionKey 会 INVALID_REQUEST。
        if (!frame.params || !("key" in frame.params) || "sessionKey" in frame.params) {
          this.#respondError(
            frame.id,
            "INVALID_REQUEST",
            "invalid sessions.messages.subscribe params: must have required property 'key'",
          )
          return
        }
        this.subscriptions.push(frame.params)
        this.#respond(frame.id, { subscribed: true })
        return
      case "chat.send":
        this.#onChatSend(frame)
        return
      case "agent.wait":
        this.#onAgentWait(frame)
        return
      case "chat.abort":
        this.aborts.push(frame.params)
        this.#respond(frame.id, { status: "aborted", runId: frame.params?.runId ?? null })
        this.#settleRun("error", "aborted")
        return
      default:
        this.#respondError(frame.id, "UNKNOWN_METHOD", `unknown method ${frame.method}`)
    }
  }

  #onChatSend(frame) {
    this.chatSendCalls.push(frame.params)
    if (this.chatError) {
      this.#respondError(
        frame.id,
        this.chatError.code ?? "INVALID_REQUEST",
        this.chatError.message ?? "chat.send rejected",
        this.chatError.details ?? null,
      )
      this.#resolveWaiters(this.chatSendWaiters)
      this.#settleRun("error", this.chatError.message ?? "chat.send rejected")
      return
    }
    this.#respond(frame.id, { runId: this.runId, status: "started" })
    this.#resolveWaiters(this.chatSendWaiters)

    // 模拟模型：先真的调一次适配器的 MCP 工具，再让 run 结束。
    setTimeout(() => {
      void this.#afterChatSend()
    }, 0)
  }

  async #afterChatSend() {
    if (this.disconnectAfterChatSend) {
      this.socket?.destroy()
      this.socket = null
      this.#settleRun("error", "gateway disconnected")
      return
    }
    if (this.toolCall && this.mcpUrl) {
      try {
        const response = await postJson(this.mcpUrl, {
          jsonrpc: "2.0",
          id: 1,
          method: "tools/call",
          params: {
            name: this.toolCall.name,
            arguments: this.toolCall.arguments ?? {},
          },
        })
        this.mcpCalls.push(response)
      } catch (error) {
        this.mcpCalls.push({ error: String(error?.message ?? error) })
      }
      this.toolDone = true
      this.#resolveWaiters(this.toolDoneWaiters)
    }

    if (this.runStatus === "ok") {
      this.#settleRun("ok")
    } else {
      this.#settleRun(this.runStatus, this.runError)
    }
  }

  #onAgentWait(frame) {
    if (this.runTerminal) {
      this.#respond(frame.id, {
        status: this.runTerminal.status,
        startedAt: this.runTerminal.startedAt,
        endedAt: this.runTerminal.endedAt,
        ...(this.runTerminal.error ? { error: this.runTerminal.error } : {}),
      })
      return
    }
    this.waiters.push({ id: frame.id })
  }

  #settleRun(status, error = "") {
    if (this.runTerminal) return
    const now = Date.now()
    this.runTerminal = { status, error, startedAt: now - 100, endedAt: now }
    for (const waiter of this.waiters) {
      this.#respond(waiter.id, {
        status: this.runTerminal.status,
        startedAt: this.runTerminal.startedAt,
        endedAt: this.runTerminal.endedAt,
        ...(error ? { error } : {}),
      })
    }
    this.waiters = []
    this.#resolveWaiters(this.runDoneWaiters)
  }

  #resolveWaiters(list) {
    const waiters = list.splice(0, list.length)
    for (const resolve of waiters) resolve()
  }

  #wait(list, timeoutMs, label, ready) {
    if (ready()) return Promise.resolve()
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`假 gateway 等 ${label} 超时`)), timeoutMs)
      list.push(() => {
        clearTimeout(timer)
        resolve()
      })
    })
  }
}

function postJson(url, body) {
  return new Promise((resolve, reject) => {
    const payload = JSON.stringify(body)
    const req = http.request(
      url,
      {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "content-length": Buffer.byteLength(payload),
        },
      },
      (res) => {
        let text = ""
        res.setEncoding("utf8")
        res.on("data", (chunk) => {
          text += chunk
        })
        res.on("end", () => {
          try {
            resolve(JSON.parse(text))
          } catch (error) {
            reject(error)
          }
        })
      },
    )
    req.on("error", reject)
    req.end(payload)
  })
}
