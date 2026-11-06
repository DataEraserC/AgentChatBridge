// 测试用的假 WorkBuddy headless CLI。
//
// 它按真实 CLI 的形状输出 stream-json：system/init → assistant → result，
// 并且真的去连适配器的 MCP server 调一次 send_message。这样测试能覆盖：
//   - turn_start → spawn CLI → MCP 工具调用（带 ticket）→ result → turn_end
//   - resume / resume 失败重试
//   - result error / 无 result / MCP 没连上 / 卡死超时
//
// 模式由 FAKE_CODEBUDDY_MODE 控制：ok / resume-missing / result-error /
// no-result / mcp-failed / hang。

import fs from "node:fs"
import http from "node:http"
import path from "node:path"

const argv = process.argv.slice(2)
const mode = process.env.FAKE_CODEBUDDY_MODE || "ok"

function valueOf(flag) {
  const index = argv.indexOf(flag)
  return index >= 0 ? argv[index + 1] ?? "" : ""
}

const resume = valueOf("-r") || valueOf("--resume")
const explicitSession = valueOf("--session-id")
const sessionId = explicitSession || resume || "fake-session"
const mcpConfig = valueOf("--mcp-config")
const prompt = argv.length > 0 ? argv[argv.length - 1] : ""

function emit(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`)
}

// 给多实例测试留痕：HOME / CODEBUDDY_CONFIG_DIR 是隔离是否生效的直接证据。
if (process.env.FAKE_CODEBUDDY_MARKER) {
  const marker = process.env.FAKE_CODEBUDDY_MARKER
  try {
    fs.mkdirSync(path.dirname(marker), { recursive: true })
    fs.writeFileSync(marker, JSON.stringify({
      home: process.env.HOME ?? "",
      configDir: process.env.CODEBUDDY_CONFIG_DIR ?? "",
      dataRoot: process.env.CODEBUDDY_DATA_ROOT ?? "",
      sessionId,
      resume: Boolean(resume),
    }))
  } catch {
    // 写不了标记不影响 CLI 本身的流程。
  }
}

function fail(message) {
  process.stderr.write(`${message}\n`)
  process.exit(1)
}

process.on("SIGTERM", () => process.exit(0))
process.on("SIGINT", () => process.exit(0))

if (mode === "resume-missing" && resume) {
  // 实测真实 CLI 的形状：JSON error 事件，然后 exit 0，没有 result。
  emit({ type: "error", error: `No conversation found with session ID: ${resume}` })
  process.exit(0)
}

if (mode === "hang") {
  emit({ type: "system", subtype: "init", session_id: sessionId, mcp_servers: [] })
  // 不结束；由适配器超时后杀掉。
  setInterval(() => {}, 60_000)
} else {
  emit({
    type: "system",
    subtype: "init",
    session_id: sessionId,
    mcp_servers: [{ name: "acb", status: mode === "mcp-failed" ? "failed" : "connected" }],
  })
  void run().catch((error) => fail(String(error?.message ?? error)))
}

async function run() {
  if (mode === "result-error") {
    emit({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "失败" }] } })
    emit({
      type: "result",
      subtype: "error_during_execution",
      is_error: true,
      result: "Rate limit exceeded: free-models-per-day",
      session_id: sessionId,
    })
    process.exit(1)
  }

  if (mode === "auth-required") {
    // 实测未登录时的 result 形状：没有 result 字段，只有 errors 数组；CLI 仍 exit 0。
    const message = "Authentication required. Please use /login command to sign in to your account"
    emit({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: message }] } })
    emit({
      type: "result",
      subtype: "error_during_execution",
      is_error: true,
      errors: [message],
      session_id: sessionId,
    })
    process.exit(0)
  }

  if (mode === "no-result") {
    emit({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "说了半句" }] } })
    fail("CLI crashed before emitting a result event")
  }

  // ok / mcp-failed：真的走一次 MCP 工具调用。
  if (mcpConfig) {
    let url = ""
    try {
      url = JSON.parse(mcpConfig).mcpServers.acb.url
    } catch {
      fail("invalid --mcp-config")
    }
    if (url) {
      await postMcp(url, {
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: { name: "send_message", arguments: { text: "收到" } },
      })
    }
  }

  if (mode === "mcp-failed") {
    // MCP 没连上时仍输出成功 result；适配器应该根据 init 事件判失败。
    emit({ type: "result", subtype: "success", is_error: false, result: "ok", session_id: sessionId })
    process.exit(0)
  }

  emit({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: "收到" }] } })
  emit({ type: "result", subtype: "success", is_error: false, result: "收到", session_id: sessionId })
  process.exit(0)
}

function postMcp(url, body) {
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

// 避免 prompt 被 lint 认为没用到；真正的 CLI 会把它交给模型。
void prompt
