// WorkBuddy 驱动器的协议层单测：假桥 + 假 headless CLI + 真 MCP server。
//
// 覆盖：
//   - turn_start → spawn CLI → MCP 工具调用（带 ticket）→ result → turn_end；
//   - resume 失败后重试、result error、无 result、MCP 没连上、超时；
//   - reset 换代次、无票时 MCP 调用被拒。
//
// 这里验的是「协议说得对」；真实 WorkBuddy 登录、额度和模型回话见 docs 的未验证清单。

import test from "node:test"
import assert from "node:assert/strict"
import http from "node:http"
import path from "node:path"

import { BridgeLink } from "../../opencode/plugin/acb-wire.mjs"
import { FakeBridge, TICKET, delivery } from "../../opencode/test/fake-bridge.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { callBridge } from "../../shared/bridge-tools.mjs"
import { CodebuddyClient } from "../src/codebuddy-client.mjs"
import { TurnDriver } from "../src/driver.mjs"

const FAKE_CLI = path.join(import.meta.dirname, "fake-codebuddy-headless.mjs")

async function connectedLink(bridge, token = "t-main") {
  const link = new BridgeLink({ endpoint: bridge.endpoint, token, harness: "workbuddy", log: () => {} })
  await link.connect()
  return link
}

function makeCli(mode) {
  return new CodebuddyClient({
    cliPath: FAKE_CLI,
    log: () => {},
    baseEnv: { ...process.env, FAKE_CODEBUDDY_MODE: mode },
  })
}

async function startMcp(driver, link) {
  const server = new McpHttpServer({
    name: "acb",
    version: "0.1.0",
    tools: () => driver.toolDefs(),
    onCall: (name, args) => callBridge(link, name, args),
    log: () => {},
  })
  const port = await server.start({ port: 0 })
  return { server, url: `http://127.0.0.1:${port}/mcp` }
}

async function nextFrameOfType(bridge, type) {
  for (;;) {
    const frame = await bridge.next()
    if (frame.frame === type) return frame
  }
}

async function waitFor(predicate, message) {
  const deadline = Date.now() + 2_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}

async function answerToolCall(bridge, text = "收到") {
  const frame = await nextFrameOfType(bridge, "tool_call")
  assert.equal(frame.frame, "tool_call")
  bridge.socket.write(`${JSON.stringify({
    frame: "tool_result",
    id: frame.id,
    result: { platform_id: "123" },
  })}\n`)
  return frame
}

test("完整一轮：turn_start → CLI → MCP 工具 → result → turn_end", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("ok")
  const driver = new TurnDriver({ cli, log: () => {}, tools: "", allowedTools: ["send_message"] })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const toolFrame = await answerToolCall(bridge)

  assert.equal(toolFrame.ticket, TICKET)
  assert.equal(toolFrame.tool, "send_message")
  assert.deepEqual(toolFrame.args, { elements: [{ type: "text", text: "收到" }] })

  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.equal(turnEnd.error, undefined)
  await run

  assert.equal(cli.runs.length, 1)
  assert.equal(cli.runs[0].sessionId, "acb-workbuddy-1-main")
  assert.equal(cli.runs[0].resume, false)
  const args = cli.runs[0].args
  assert.ok(args.includes("--output-format"))
  assert.ok(args.includes("stream-json"))
  assert.ok(args.includes("--strict-mcp-config"))
  assert.equal(args[args.indexOf("--mcp-config") + 1], JSON.stringify({ mcpServers: { acb: { type: "http", url: mcp.url } } }))
  assert.equal(args[args.indexOf("--tools") + 1], "")
})

test("票据校验：没有轮次时 MCP 工具调用被桥拒绝", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("ok")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  assert.equal(link.ticket, null)
  const response = await postMcp(mcp.url, {
    jsonrpc: "2.0",
    id: 1,
    method: "tools/call",
    params: { name: "send_message", arguments: { text: "偷跑" } },
  })
  assert.equal(response.result.isError, true)
  assert.match(response.result.content[0].text, /unauthorized|no turn is in progress/i)
})

test("result error：turn_end 带 rate_limited 分类", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("result-error")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.match(turnEnd.error, /Rate limit/)
  assert.deepEqual(turnEnd.failure, { component: "model", class: "rate_limited" })
  await run
})

test("未登录：真实 CLI 的 errors 数组形状被识别为 auth_failed", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("auth-required")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.match(turnEnd.error, /Authentication required/)
  assert.deepEqual(turnEnd.failure, { component: "model", class: "auth_failed" })
  await run
})

test("CLI 没有 result 事件就退出：按失败上报", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("no-result")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.match(turnEnd.error, /crashed|exited/i)
  await run
})

test("MCP 没连上：CLI 即使 result 成功，这一轮也判失败", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("mcp-failed")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  // 假 CLI 仍会调一次 MCP，先把工具调用回掉，才能走到 exit。
  await answerToolCall(bridge)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.match(turnEnd.error, /MCP server did not connect/i)
  assert.deepEqual(turnEnd.failure, { component: "backend", class: "mcp_unavailable" })
  await run
})

test("第二轮复用同一 CLI 会话（-r），不再新建", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("ok")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const first = driver.run(delivery(), link)
  await answerToolCall(bridge)
  await first
  await nextFrameOfType(bridge, "turn_end")

  link.ticket = "main-2"
  const second = driver.run(delivery({ ticket: "main-2" }), link)
  await waitFor(() => cli.runs.length === 2, "第二轮 CLI 没起")
  await answerToolCall(bridge)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, "main-2")
  assert.equal(turnEnd.error, undefined)
  await second

  assert.equal(cli.runs[1].resume, true)
  assert.equal(cli.runs[1].sessionId, cli.runs[0].sessionId)
})

test("resume 失败：丢掉旧映射，用 --session-id 重试成功", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("resume-missing")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  // 第一轮：没有 resume，正常建立会话。
  link.ticket = TICKET
  const first = driver.run(delivery(), link)
  await answerToolCall(bridge)
  await first
  await nextFrameOfType(bridge, "turn_end")

  // 第二轮：先 -r 失败，驱动改成 --session-id 重试。
  link.ticket = "main-2"
  const second = driver.run(delivery({ ticket: "main-2" }), link)
  await answerToolCall(bridge)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.error, undefined)
  await second

  assert.equal(cli.runs.length, 3, "第二次开轮应有一次 -r 失败 + 一次 --session-id 重试")
  assert.equal(cli.runs[1].resume, true)
  assert.equal(cli.runs[2].resume, false)
  assert.equal(cli.runs[2].sessionId, cli.runs[1].sessionId)
})

test("turn_timeout_ms 到点：杀掉 CLI 并回失败", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("hang")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery({ turn_timeout_ms: 100 }), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.match(turnEnd.error, /did not finish|timeout/i)
  assert.deepEqual(turnEnd.failure, { component: "backend", class: "timeout" })
  await run
})

test("reset 后下一轮用新 id 且不是 resume", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  const cli = makeCli("ok")
  const driver = new TurnDriver({ cli, log: () => {} })
  const mcp = await startMcp(driver, link)
  driver.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const first = driver.run(delivery(), link)
  await answerToolCall(bridge)
  await first
  await nextFrameOfType(bridge, "turn_end")

  driver.resetSession("main")
  link.ticket = "main-2"
  const second = driver.run(delivery({ ticket: "main-2" }), link)
  await waitFor(() => cli.runs.length === 2, "第二轮 CLI 没起")
  await answerToolCall(bridge)
  await second
  await nextFrameOfType(bridge, "turn_end")
  assert.equal(cli.runs[1].resume, false)
  assert.match(cli.runs[1].sessionId, /^acb-workbuddy-1-main-r\w+$/)
})

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
