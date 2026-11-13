// 测试用的假 ACP agent：只把协议说对，不接任何真实模型。
//
// 它同时充当「声明式启动命令」的活证据：测试里把 `node test/fake-agent.mjs`
// 当成 argv 交给适配器，适配器不需要知道它是什么，只要它会说 ACP。
//
// 行为由环境变量选：
//   FAKE_ACP_MODE=ok|permission|error|crash|slow|mcp
//   FAKE_ACP_VERSION=1            换协议版本（测版本不匹配）
//   FAKE_ACP_KNOWN=sess-1,sess-2  session/load 认这些 id
//
// stderr 打的是给人看的方法流水，测试据此断言「调了哪些方法」。

import process from "node:process"

const mode = process.env.FAKE_ACP_MODE || "ok"
const version = Number(process.env.FAKE_ACP_VERSION || 1)
const known = new Set(
  (process.env.FAKE_ACP_KNOWN || "")
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean),
)

let buffer = ""
let counter = 0
let nextId = 1
const sessions = new Map()
const pendingRequests = new Map()
const prompts = new Map()

const write = (frame) => process.stdout.write(`${JSON.stringify(frame)}\n`)
const note = (line) => process.stderr.write(`[fake-acp] ${line}\n`)
const notify = (method, params) => write({ jsonrpc: "2.0", method, params })

function request(method, params) {
  const id = `fake-${nextId++}`
  write({ jsonrpc: "2.0", id, method, params })
  return new Promise((resolve, reject) => {
    pendingRequests.set(id, { resolve, reject })
  })
}

function update(sessionId, payload) {
  notify("session/update", { sessionId, update: payload })
}

async function handlePrompt(message) {
  const { sessionId, prompt } = message.params ?? {}
  const text = (prompt ?? [])
    .filter((item) => item?.type === "text")
    .map((item) => item.text)
    .join("\n")
  note(`prompt session=${sessionId} bytes=${Buffer.byteLength(text)} mode=${mode}`)
  const session = sessions.get(sessionId)
  if (session) session.promptText = text

  if (mode === "error") {
    write({
      jsonrpc: "2.0",
      id: message.id,
      error: { code: -32000, message: "rate limit exceeded, please retry later" },
    })
    return
  }
  if (mode === "crash") {
    process.exit(1)
  }

  const finish = (result) => write({ jsonrpc: "2.0", id: message.id, result })
  const done = { stopReason: "end_turn" }

  if (mode === "slow") {
    // 等 session/cancel：谁先到谁说话。
    prompts.set(sessionId, () => finish({ stopReason: "cancelled" }))
    return
  }

  update(sessionId, { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "收到" } })

  if (mode === "permission") {
    const answer = await request("session/request_permission", {
      sessionId,
      toolCall: { toolCallId: "call-1", title: "Bash", kind: "execute" },
      options: [
        { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
        { optionId: "reject-once", name: "Reject", kind: "reject_once" },
      ],
    })
    const chosen = answer?.result?.outcome?.optionId ?? answer?.result?.outcome?.outcome ?? "none"
    note(`permission chosen=${chosen}`)
    finish({ ...done, _meta: { chosen } })
    return
  }

  if (mode === "mcp") {
    const server = (session?.mcpServers ?? [])[0] ?? null
    const url = server?.url ?? ""
    let toolResult = "no-mcp"
    if (url) {
      try {
        const response = await fetch(url, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: 900,
            method: "tools/call",
            params: { name: "send_message", arguments: { text: "假 agent 调用了一次桥工具" } },
          }),
        })
        const body = await response.json()
        toolResult = body?.result?.content?.[0]?.text ?? JSON.stringify(body)
      } catch (error) {
        toolResult = `mcp-error:${error?.message ?? error}`
      }
    }
    finish({ ...done, _meta: { toolResult } })
    return
  }

  finish(done)
}

function handle(message) {
  if (!message || typeof message !== "object") return
  if (message.method && message.id !== undefined && message.id !== null) {
    note(`request ${message.method}`)
    switch (message.method) {
      case "initialize":
        write({
          jsonrpc: "2.0",
          id: message.id,
          result: {
            protocolVersion: version,
            agentCapabilities: {
              loadSession: true,
              sessionCapabilities: { close: {}, list: {} },
              promptCapabilities: { image: true, embeddedContext: true },
            },
            authMethods: [{ id: "fake", name: "Fake login" }],
          },
        })
        return
      case "session/new": {
        counter += 1
        const sessionId = `sess-${counter}`
        // ACP 只在建会话/载入时收 mcpServers，prompt 里没有：先存下来给 mcp 模式用。
        sessions.set(sessionId, {
          promptText: "",
          mcpServers: message.params?.mcpServers ?? [],
        })
        // 会话一建好就推一条 config 更新：适配器必须忽略它而不是当成帧错误。
        update(sessionId, { sessionUpdate: "config_option_update", configOptions: [] })
        write({ jsonrpc: "2.0", id: message.id, result: { sessionId, models: { availableModels: [] } } })
        return
      }
      case "session/load":
        if (!known.has(message.params?.sessionId)) {
          write({
            jsonrpc: "2.0",
            id: message.id,
            error: { code: -32002, message: `unknown session: ${message.params?.sessionId}` },
          })
          return
        }
        sessions.set(message.params.sessionId, {
          promptText: "",
          mcpServers: message.params?.mcpServers ?? [],
        })
        write({ jsonrpc: "2.0", id: message.id, result: {} })
        return
      case "session/close":
        sessions.delete(message.params?.sessionId)
        write({ jsonrpc: "2.0", id: message.id, result: {} })
        return
      case "session/prompt":
        void handlePrompt(message)
        return
      case "authenticate":
        write({ jsonrpc: "2.0", id: message.id, result: {} })
        return
      default:
        write({
          jsonrpc: "2.0",
          id: message.id,
          error: { code: -32601, message: `method not found: ${message.method}` },
        })
    }
    return
  }
  if (message.id !== undefined && message.id !== null && (message.result !== undefined || message.error !== undefined)) {
    const entry = pendingRequests.get(message.id)
    if (entry) {
      pendingRequests.delete(message.id)
      entry.resolve(message)
    }
    return
  }
  if (message.method === "session/cancel") {
    note(`cancel session=${message.params?.sessionId}`)
    const finish = prompts.get(message.params?.sessionId)
    if (finish) {
      prompts.delete(message.params?.sessionId)
      finish()
    }
  }
}

process.stdin.setEncoding("utf8")
process.stdin.on("data", (chunk) => {
  buffer += chunk
  let index
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index).trim()
    buffer = buffer.slice(index + 1)
    if (!line) continue
    try {
      handle(JSON.parse(line))
    } catch (error) {
      note(`bad frame: ${error?.message ?? error}`)
    }
  }
})
process.stdin.on("end", () => process.exit(0))
