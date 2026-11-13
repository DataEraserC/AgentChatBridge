// 轮次驱动测试：假桥 + 假 ACP agent，走真实子进程和真实 MCP HTTP。

import assert from "node:assert/strict"
import test from "node:test"
import { fileURLToPath } from "node:url"

import { delivery, surface } from "../../opencode/test/fake-bridge.mjs"
import { TurnDriver, classifyFailureText, turnTimeout } from "../src/driver.mjs"

const FAKE_AGENT = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url))

function agent(mode = "ok", known = "") {
  return {
    id: "fake",
    acp: [process.execPath, FAKE_AGENT],
    env: { FAKE_ACP_MODE: mode, FAKE_ACP_KNOWN: known },
  }
}

/** 记录帧的假桥：只实现 driver 真正会调的那几个方法。 */
function stubLink() {
  return {
    ticket: "t-1",
    turnCalls: 0,
    sessions: ["main"],
    sessionMaps: [],
    calls: [],
    ends: [],
    controls: [],
    async call(name, args) {
      this.turnCalls += 1
      this.calls.push({ name, args })
      if (name === "capabilities" && args?.scope === "surface") return surface()
      return JSON.stringify({ ok: true })
    },
    endTurn(ticket, error, failure) {
      this.ends.push({ ticket, error, failure })
    },
    controlResult(frame) {
      this.controls.push(frame)
    },
  }
}

async function startDriver(t, { mode = "ok", known = "", permission = "allow" } = {}) {
  const logs = []
  const { acp } = agent(mode, known)
  const driver = new TurnDriver({
    agent: { id: "fake", acp },
    config: {
      harness: "fake",
      cwd: "/tmp",
      permission,
      appendSystemPrompt: "[规则]",
      env: { ...process.env, FAKE_ACP_MODE: mode, FAKE_ACP_KNOWN: known },
    },
    log: (level, message) => logs.push(`${level} ${message}`),
  })
  driver.mcpUrl = "http://127.0.0.1:1/mcp"
  t.after(async () => {
    await driver.stop()
  })
  return { driver, logs }
}

function message(id, text) {
  return {
    id,
    room: { connection: "qq", path: ["group", "1"] },
    kind: { kind: "message" },
    sender: { id: "10001", name: "老王" },
    at: Date.now(),
    elements: [{ type: "text", text }],
  }
}

test("一轮跑完：回报 turn_end，并记下 harness 会话", async (t) => {
  const { driver } = await startDriver(t)
  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "在吗")] }), link)

  assert.equal(link.ends.length, 1)
  assert.equal(link.ends[0].error, null)
  assert.equal(link.ends[0].ticket, delivery().ticket)
  const map = link.controls.find((frame) => frame.command === "report_map")
  assert.ok(map, "要把 harness 会话回报给桥")
  assert.match(map.result.session_id, /^sess-/)
  assert.equal(driver.sessions.get("main"), map.result.session_id)
})

test("agent 通过 MCP 调桥工具：工具名走到桥，turnCalls 有数", async (t) => {
  const { driver } = await startDriver(t, { mode: "mcp" })
  // 起真实的 MCP HTTP server，假 agent 会 POST tools/call 进来。
  const { McpHttpServer } = await import("../../dsh/src/mcp-http-server.mjs")
  const server = new McpHttpServer({
    tools: () => driver.toolDefs(),
    onCall: async (name, args) => {
      server.called = { name, args }
      return "发好了"
    },
  })
  const port = await server.start({ port: 0 })
  t.after(() => server.stop())
  driver.mcpUrl = `http://127.0.0.1:${port}/mcp`

  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "在吗")] }), link)

  assert.equal(link.ends[0].error, null)
  assert.equal(server.called?.name, "send_message")
  assert.match(server.called?.args?.text ?? "", /桥工具/)
})

test("权限策略 reject 时不给 allow", async (t) => {
  const { driver, logs } = await startDriver(t, { mode: "permission", permission: "reject" })
  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "跑个命令")] }), link)
  assert.equal(link.ends[0].error, null)
  assert.ok(
    logs.some((line) => line.includes("permission") && line.includes("Reject")),
    `要选 reject 选项，实际日志：${logs.join("\n")}`,
  )
})

test("模型限流：turn_end 带失败分类", async (t) => {
  const { driver } = await startDriver(t, { mode: "error" })
  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "在吗")] }), link)
  assert.match(link.ends[0].error, /rate limit/)
  assert.deepEqual(link.ends[0].failure, { component: "model", class: "rate_limited" })
})

test("打断：session/cancel 之后这一轮以 cancelled 收场", async (t) => {
  const { driver } = await startDriver(t, { mode: "slow" })
  const link = stubLink()
  const running = driver.run(delivery({ records: [message(12, "慢活")] }), link)
  // 等会话建好再打断。
  for (let i = 0; i < 100 && driver.live.size === 0; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
  await driver.handleControl({ id: "c-1", session: "main", command: "interrupt" }, link)
  await running
  // 文案随语言变，断言「有失败 + 是取消语义」这件事本身。
  assert.match(link.ends[0].error, /cancel|打断/i)
  assert.equal(
    link.controls.some((frame) => frame.command === "interrupt" && frame.result?.ok),
    true,
  )
})

test("已有 harness 会话时先 session/load，不新建", async (t) => {
  const { driver, logs } = await startDriver(t, { known: "sess-known" })
  driver.seedMaps([{ session: "main", harness_session: "sess-known" }])
  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "接着说")] }), link)
  assert.equal(link.ends[0].error, null)
  assert.equal(driver.sessions.get("main"), "sess-known")
  assert.ok(
    logs.some((line) => line.includes("session/load")),
    `应当走 session/load，实际日志：${logs.join("\n")}`,
  )
})

test("重置：丢掉映射并给下一轮新建会话", async (t) => {
  const { driver } = await startDriver(t)
  const link = stubLink()
  await driver.run(delivery({ records: [message(12, "第一轮")] }), link)
  const first = driver.sessions.get("main")

  await driver.handleControl({ id: "c-2", session: "main", command: "reset" }, link)
  assert.equal(driver.sessions.has("main"), false)
  assert.equal(driver.live.size, 0)

  await driver.run(delivery({ records: [message(13, "第二轮")] }), link)
  assert.notEqual(driver.sessions.get("main"), first)
})

test("超时语义：桥给 0 就是不设超时", () => {
  assert.equal(turnTimeout({ turn_timeout_ms: 0 }), 0)
  assert.equal(turnTimeout({ turn_timeout_ms: 1234 }), 1234)
  assert.ok(turnTimeout({}) > 0)
})

test("错误分类认得鉴权与内容审核", () => {
  assert.deepEqual(classifyFailureText("401 unauthorized"), { component: "model", class: "auth_failed" })
  assert.deepEqual(classifyFailureText("content-blocked by upstream"), {
    component: "model",
    class: "model_unavailable",
  })
  assert.equal(classifyFailureText("something odd"), null)
})
