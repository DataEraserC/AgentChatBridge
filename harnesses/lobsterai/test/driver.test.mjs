// LobsterAI 驱动器的协议层单测：假桥 + 假 gateway（真 OpenClawGateway 客户端）。
//
// 覆盖：
//   - turn_start → chat.send → MCP 工具调用 → agent.wait ok → turn_end；
//   - 工具调用带当前 ticket；没有轮次时 MCP 调用被拒；
//   - chat.send 拒绝 / run error / gateway 断开 / 超时打断；
//   - reset 后代次换 key。
//
// 这里验的是「协议说得对」；真实 LobsterAI 登录、额度和模型回话见 docs 的未验证清单。

import test from "node:test"
import assert from "node:assert/strict"
import http from "node:http"

import { BridgeLink } from "../../opencode/plugin/acb-wire.mjs"
import { FakeBridge, TICKET, delivery } from "../../opencode/test/fake-bridge.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { callBridge } from "../../shared/bridge-tools.mjs"
import { TurnDriver } from "../src/driver.mjs"
import { OpenClawGateway } from "../src/openclaw-gateway.mjs"
import { FakeGateway } from "./fake-gateway.mjs"

async function connectedLink(bridge, token = "t-main") {
  const link = new BridgeLink({ endpoint: bridge.endpoint, token, harness: "lobsterai", log: () => {} })
  await link.connect()
  return link
}

async function connectGateway(fake) {
  const gateway = new OpenClawGateway({ url: fake.url, token: fake.token, log: () => {} })
  await gateway.connect()
  return gateway
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

test("完整一轮：turn_start → chat.send → MCP 工具 → agent.wait ok → turn_end", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway({
    toolCall: { name: "send_message", arguments: { text: "收到" } },
  }).start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })
  const mcp = await startMcp(driver, link)
  fake.mcpUrl = mcp.url

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    mcp.server.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)

  await fake.waitForChatSend()
  // 真 gateway 只认注册表里的 client id / mode（假 gateway 也照着校验）。
  const connectReq = fake.requests.find((frame) => frame.method === "connect")
  assert.equal(connectReq.params.client.id, "gateway-client")
  assert.equal(connectReq.params.client.mode, "backend")
  // transcript 订阅的参数名是 key，不是 sessionKey。
  await waitFor(() => fake.subscriptions.length > 0, "gateway 没收到 transcript 订阅")
  assert.deepEqual(fake.subscriptions[0], { key: "acb-main" })
  assert.equal(fake.chatSendCalls.length, 1)
  assert.equal(fake.chatSendCalls[0].sessionKey, "acb-main")
  assert.equal(fake.chatSendCalls[0].idempotencyKey, TICKET)
  assert.match(fake.chatSendCalls[0].message, /在吗/)
  assert.match(fake.chatSendCalls[0].message, /mcp__acb__send_message/)

  // 假模型通过 MCP 调 send_message，适配器把它变成带票的 tool_call。
  const toolFrame = await nextFrameOfType(bridge, "tool_call")
  assert.equal(toolFrame.ticket, TICKET)
  assert.equal(toolFrame.tool, "send_message")
  assert.deepEqual(toolFrame.args, { elements: [{ type: "text", text: "收到" }] })

  bridge.socket.write(`${JSON.stringify({
    frame: "tool_result",
    id: toolFrame.id,
    result: { platform_id: "123" },
  })}\n`)

  await fake.waitForToolDone()
  assert.equal(fake.mcpCalls.length, 1)
  assert.match(fake.mcpCalls[0].result.content[0].text, /123/)

  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.equal(turnEnd.error, undefined)
  await run
})

test("票据校验：没有轮次时 MCP 工具调用被桥拒绝", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway().start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })
  const mcp = await startMcp(driver, link)

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
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

  // 直接走 BridgeLink 也要被拒，适配器没有旁路。
  await assert.rejects(
    () => link.call("send_message", { elements: [{ type: "text", text: "偷跑" }] }),
    /no turn is in progress|cannot be called/,
  )
})

test("chat.send 被 gateway 拒绝：turn_end 带错误，不假装成功", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway({
    chatError: { code: "UNAUTHORIZED", message: "login required" },
  }).start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.match(turnEnd.error, /login required/)
  assert.deepEqual(turnEnd.failure, { component: "backend", class: "auth_failed" })
  await run
})

test("run 以 error 结束：turn_end 带 rate_limited 分类", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway({
    runStatus: "error",
    runError: "Rate limit exceeded: free-models-per-day",
  }).start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.match(turnEnd.error, /Rate limit/)
  assert.deepEqual(turnEnd.failure, { component: "model", class: "rate_limited" })
  await run
})

test("gateway 中途断开：这一轮立即失败，不挂死", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway({ disconnectAfterChatSend: true }).start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery(), link)
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.match(turnEnd.error, /closed|disconnect/i)
  assert.deepEqual(turnEnd.failure, { component: "backend", class: "closed" })
  await run
})

test("turn_timeout_ms 到点未结束：chat.abort 打断并回失败", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway({ runStatus: "timeout" }).start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const run = driver.run(delivery({ turn_timeout_ms: 50 }), link)
  await waitFor(() => fake.aborts.length > 0, "超时后应调 chat.abort")
  const turnEnd = await nextFrameOfType(bridge, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.match(turnEnd.error, /timed out|timeout/i)
  assert.deepEqual(turnEnd.failure, { component: "backend", class: "timeout" })
  await run
})

test("reset 后换带代次的新 session key，旧轮次不受影响", async (t) => {
  const bridge = await new FakeBridge().start()
  const fake = await new FakeGateway().start()
  const gateway = await connectGateway(fake)
  const link = await connectedLink(bridge)
  const driver = new TurnDriver({ gateway, log: () => {} })

  t.after(async () => {
    link.close()
    gateway.close()
    await fake.stop()
    await bridge.stop()
  })

  link.ticket = TICKET
  const first = driver.run(delivery(), link)
  await fake.waitForChatSend()
  fake.completeRun("ok")
  await first
  await nextFrameOfType(bridge, "turn_end")

  driver.resetSession("main")
  link.ticket = "main-2"
  const second = driver.run(delivery({ ticket: "main-2" }), link)
  await waitFor(() => fake.chatSendCalls.length === 2, "第二轮 chat.send 没到")
  assert.equal(fake.chatSendCalls.length, 2)
  assert.match(fake.chatSendCalls[1].sessionKey, /^acb-main-r\w+$/)
  fake.completeRun("ok")
  await second
  await nextFrameOfType(bridge, "turn_end")
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
