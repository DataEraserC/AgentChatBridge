// ACP（Agent Client Protocol）客户端：stdio 上的 JSON-RPC 2.0，一行一条消息。
//
// 只实现 ACB 用得到的那部分协议：
//   - 请求：initialize / session/new / session/load / session/prompt /
//           session/close / authenticate
//   - 通知：session/cancel（我们发）、session/update（对方发）
//   - 对方的请求：session/request_permission 必须答；fs/*、terminal/* 我们
//     在 initialize 里声明不支持，收到就回 method not found。
//
// 为什么不用 SDK：协议只有「请求/响应/通知」三种帧，自己实现不到两百行，还能
// 精确控制超时、子进程回收和错误归类；多引一个依赖反而要在 nix 里再加一层。
//
// 帧格式：每条消息一行 JSON，末尾 \n。stdout 是唯一数据通道，日志一律走 stderr。

import { spawn } from "node:child_process"
import { t } from "../../shared/i18n.mjs"

/** 协商的 ACP 协议版本。对方回别的版本直接拒绝，不猜兼容性。 */
export const PROTOCOL_VERSION = 1

const INITIALIZE_TIMEOUT_MS = 30_000
const SESSION_TIMEOUT_MS = 120_000
const CLOSE_TIMEOUT_MS = 5_000

/** ACP 层的错误：JSON-RPC 的 code/data 原样保留，外加出错的方法名。 */
export class AcpError extends Error {
  constructor(code, message, { method = "", data = null } = {}) {
    super(message)
    this.name = "AcpError"
    this.code = code
    this.method = method
    this.data = data
    this.kind = kindForCode(code)
  }
}

/** 把错误码归成适配器关心的几类；认不出就是协议错误。 */
function kindForCode(code) {
  if (code === "timeout") return "timeout"
  if (code === "agent_exited") return "unreachable"
  if (code === "spawn_failed") return "unreachable"
  if (code === "unsupported_version") return "unsupported_version"
  if (code === -32000) return "agent_error"
  return "rpc_error"
}

/**
 * 一条 ACP 连接 = 一个子进程 + 一套未完成请求表。
 *
 * `onNotification(message)` 收 `session/update` 等通知；
 * `onRequest(message)` 返回 result（或抛 AcpError）来应答对方的请求。
 */
export class AcpConnection {
  constructor({
    argv,
    cwd = process.cwd(),
    env = process.env,
    log = () => {},
    onNotification = () => {},
    onRequest = null,
    requestTimeoutMs = 0,
  } = {}) {
    if (!Array.isArray(argv) || argv.length === 0) {
      throw new AcpError("spawn_failed", t("acp.command_missing"))
    }
    this.argv = argv
    this.cwd = cwd
    this.env = env
    this.log = log
    this.onNotification = onNotification
    this.onRequest = onRequest
    this.requestTimeoutMs = requestTimeoutMs

    this.child = null
    this.buffer = ""
    this.pending = new Map()
    this.seq = 0
    this.closed = false
    this.capabilities = null
    this.agentInfo = null
    this.authMethods = []
    this.protocolVersion = null
    this.stderr = []
    this.exit = null
  }

  /** 起进程。返回后 stdout 就能收帧；退出会拒绝所有未完成请求。 */
  async start() {
    if (this.child) return this
    const [program, ...args] = this.argv
    let child
    try {
      child = spawn(program, args, {
        cwd: this.cwd,
        env: this.env,
        stdio: ["pipe", "pipe", "pipe"],
      })
    } catch (error) {
      throw new AcpError("spawn_failed", t("acp.spawn_failed", {
        command: this.argv.join(" "),
        error: error?.message ?? error,
      }))
    }
    this.child = child

    child.on("error", (error) => {
      this.#rejectAll(new AcpError("spawn_failed", t("acp.spawn_failed", {
        command: this.argv.join(" "),
        error: error?.message ?? error,
      })))
    })
    child.on("exit", (code, signal) => {
      this.exit = { code, signal }
      this.closed = true
      this.#rejectAll(new AcpError("agent_exited", t("acp.agent_exited", {
        code: code ?? "null",
        signal: signal ?? "none",
        stderr: this.stderrTail(),
      })))
    })
    child.stdout.setEncoding("utf8")
    child.stdout.on("data", (chunk) => this.#feed(chunk))
    child.stderr.setEncoding("utf8")
    child.stderr.on("data", (chunk) => {
      for (const line of String(chunk).split("\n")) {
        if (!line.trim()) continue
        this.stderr.push(line)
        if (this.stderr.length > 200) this.stderr.shift()
        this.log("debug", t("acp.agent_stderr", { line: line.slice(0, 400) }))
      }
    })
    // 子进程已经退出时再写 stdin 会 EPIPE：接住它，别把整个进程带崩。
    child.stdin.on("error", () => {})
    return this
  }

  get pid() {
    return this.child?.pid ?? 0
  }

  /** 对方是否支持 `session/close`（能力里显式声明才算）。 */
  supportsClose() {
    return Boolean(this.capabilities?.sessionCapabilities?.close)
  }

  stderrTail(limit = 800) {
    const text = this.stderr.join("\n")
    return text.length > limit ? `…${text.slice(-limit)}` : text
  }

  #feed(chunk) {
    this.buffer += chunk
    let index
    while ((index = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, index).trim()
      this.buffer = this.buffer.slice(index + 1)
      if (!line) continue
      let message
      try {
        message = JSON.parse(line)
      } catch {
        this.log("warn", t("acp.bad_frame", { text: line.slice(0, 200) }))
        continue
      }
      this.#dispatch(message)
    }
  }

  #dispatch(message) {
    if (!message || typeof message !== "object") return
    if (message.id !== undefined && message.id !== null && (message.result !== undefined || message.error !== undefined)) {
      const entry = this.pending.get(message.id)
      if (!entry) {
        this.log("debug", t("acp.unknown_response", { id: String(message.id) }))
        return
      }
      this.pending.delete(message.id)
      clearTimeout(entry.timer)
      if (message.error) {
        entry.reject(new AcpError(message.error.code ?? "rpc_error", message.error.message ?? "agent error", {
          method: entry.method,
          data: message.error.data ?? null,
        }))
      } else {
        entry.resolve(message.result ?? {})
      }
      return
    }
    if (typeof message.method === "string") {
      if (message.id !== undefined && message.id !== null) {
        // 对方发来的请求：必须应答，否则它会一直等。
        this.#answer(message)
      } else {
        this.onNotification(message)
      }
    }
  }

  async #answer(message) {
    const reply = (result, error) => {
      if (this.closed) return
      const frame = error
        ? { jsonrpc: "2.0", id: message.id, error: { code: error.code ?? -32603, message: error.message, data: error.data ?? null } }
        : { jsonrpc: "2.0", id: message.id, result }
      this.#write(frame)
    }
    if (!this.onRequest) {
      reply(null, { code: -32601, message: `method not found: ${message.method}` })
      return
    }
    try {
      const result = await this.onRequest(message)
      reply(result ?? {})
    } catch (error) {
      reply(null, {
        code: typeof error?.code === "number" ? error.code : -32603,
        message: error?.message ?? String(error),
        data: error?.data ?? null,
      })
    }
  }

  #write(frame) {
    if (!this.child || this.closed) return
    try {
      this.child.stdin.write(`${JSON.stringify(frame)}\n`)
    } catch (error) {
      this.log("warn", t("acp.write_failed", { error: error?.message ?? error }))
    }
  }

  #request(method, params, timeoutMs) {
    if (this.closed) {
      return Promise.reject(new AcpError("agent_exited", t("acp.agent_exited", {
        code: this.exit?.code ?? "null",
        signal: this.exit?.signal ?? "none",
        stderr: this.stderrTail(),
      }), { method }))
    }
    const id = ++this.seq
    return new Promise((resolve, reject) => {
      const timer = timeoutMs > 0
        ? setTimeout(() => {
            this.pending.delete(id)
            reject(new AcpError("timeout", t("acp.request_timeout", { method, ms: timeoutMs }), { method }))
          }, timeoutMs)
        : null
      this.pending.set(id, { method, resolve, reject, timer })
      this.#write({ jsonrpc: "2.0", id, method, params })
    })
  }

  #notify(method, params) {
    this.#write({ jsonrpc: "2.0", method, params })
  }

  #rejectAll(error) {
    for (const [id, entry] of this.pending) {
      this.pending.delete(id)
      clearTimeout(entry.timer)
      entry.reject(new AcpError(error.code, error.message, { method: entry.method, data: error.data }))
    }
  }

  /**
   * 握手。`clientCapabilities` 固定声明「不做文件读写、不开终端」——桥只提供
   * MCP 工具，agent 自己的工具面由它自己决定；声明不支持就别假装能做。
   */
  async initialize({ clientInfo = { name: "acb-acp", version: "0.1.0" }, meta = null } = {}) {
    const params = {
      protocolVersion: PROTOCOL_VERSION,
      clientCapabilities: {
        fs: { readTextFile: false, writeTextFile: false },
        terminal: false,
      },
      clientInfo,
    }
    if (meta !== null) params._meta = meta
    const result = await this.#request("initialize", params, INITIALIZE_TIMEOUT_MS)
    const version = result?.protocolVersion
    if (version !== PROTOCOL_VERSION) {
      throw new AcpError("unsupported_version", t("acp.unsupported_version", {
        theirs: String(version),
        ours: String(PROTOCOL_VERSION),
      }), { method: "initialize" })
    }
    this.protocolVersion = version
    this.capabilities = result.agentCapabilities ?? {}
    this.agentInfo = result.agentInfo ?? null
    this.authMethods = Array.isArray(result.authMethods) ? result.authMethods : []
    return result
  }

  /** 需要登录时由调用方指名认证方式（码在 authMethods 里）。 */
  async authenticate(methodId) {
    return this.#request("authenticate", { methodId }, INITIALIZE_TIMEOUT_MS)
  }

  /** 新会话。`mcpServers` 是 ACP 的声明式 MCP 列表（http / stdio）。 */
  async sessionNew({ cwd, mcpServers = [] }) {
    const result = await this.#request("session/new", { cwd, mcpServers }, SESSION_TIMEOUT_MS)
    if (!result?.sessionId) {
      throw new AcpError("bad_response", t("acp.session_id_missing"), { method: "session/new" })
    }
    return result
  }

  /** 载入历史会话；对方不支持时抛 `unsupported`，调用方退回 session/new。 */
  async sessionLoad({ sessionId, cwd, mcpServers = [] }) {
    const result = await this.#request("session/load", { sessionId, cwd, mcpServers }, SESSION_TIMEOUT_MS)
    return result ?? {}
  }

  /** 设置会话配置项（模型/模式/思考等级）；`value` 是选项 id。 */
  async setConfigOption(sessionId, configId, value) {
    return this.#request("session/set_config_option", { sessionId, configId, value }, SESSION_TIMEOUT_MS)
  }

  /**
   * 投一轮话。`timeoutMs` = 0 表示不设超时（桥的 `turn_timeout_secs = 0`）。
   * 返回 `{ stopReason, ... }`；被打断时 stopReason 是 `cancelled`。
   */
  async prompt({ sessionId, content, timeoutMs = 0 }) {
    return this.#request("session/prompt", { sessionId, prompt: content }, timeoutMs)
  }

  /** 打断当前轮（通知，不等待响应）。 */
  cancel(sessionId) {
    this.#notify("session/cancel", { sessionId })
  }

  /** 关闭会话（对方支持 session/close 时才有意义）。 */
  async closeSession(sessionId) {
    return this.#request("session/close", { sessionId }, CLOSE_TIMEOUT_MS)
  }

  /** 停进程：先 SIGTERM，等不到就 SIGKILL。返回后不会再有任何帧。 */
  async stop({ timeoutMs = 3000 } = {}) {
    if (!this.child) return
    this.closed = true
    this.#rejectAll(new AcpError("agent_exited", t("acp.stopped")))
    const child = this.child
    this.child = null
    if (child.exitCode !== null || child.signalCode !== null) return
    const exited = new Promise((resolve) => child.once("exit", resolve))
    try {
      child.kill("SIGTERM")
    } catch {
      // 已经没了。
    }
    const timer = setTimeout(() => {
      try {
        child.kill("SIGKILL")
      } catch {
        // 同上。
      }
    }, timeoutMs)
    await exited
    clearTimeout(timer)
  }
}
