// ACP 客户端测试：用真实子进程讲协议，不 mock stdout。

import assert from "node:assert/strict"
import test from "node:test"
import { fileURLToPath } from "node:url"

import { AcpConnection, AcpError } from "../src/client.mjs"

const FAKE_AGENT = fileURLToPath(new URL("./fake-agent.mjs", import.meta.url))

function connection(t, env = {}) {
  const updates = []
  const conn = new AcpConnection({
    argv: [process.execPath, FAKE_AGENT],
    env: { ...process.env, ...env },
    log: () => {},
    onNotification: (message) => updates.push(message),
    onRequest: async (message) => {
      if (message.method !== "session/request_permission") {
        throw new AcpError(-32601, `method not found: ${message.method}`, { method: message.method })
      }
      return { outcome: { outcome: "selected", optionId: "allow-once" } }
    },
  })
  t.after(async () => {
    await conn.stop()
  })
  return { conn, updates }
}

test("握手、建会话、投一轮：通知收得到，结果带 stopReason", async (t) => {
  const { conn, updates } = connection(t)
  await conn.start()
  const init = await conn.initialize()
  assert.equal(init.protocolVersion, 1)
  assert.equal(conn.capabilities.loadSession, true)
  assert.equal(conn.supportsClose(), true)

  const created = await conn.sessionNew({ cwd: "/tmp", mcpServers: [] })
  assert.match(created.sessionId, /^sess-/)

  const result = await conn.prompt({
    sessionId: created.sessionId,
    content: [{ type: "text", text: "在吗" }],
  })
  assert.equal(result.stopReason, "end_turn")
  const kinds = updates.map((message) => message.params?.update?.sessionUpdate)
  assert.ok(kinds.includes("config_option_update"), "建会话时推的更新不能被当成坏帧")
  assert.ok(kinds.includes("agent_message_chunk"))
})

test("权限请求由 onRequest 应答，选项原样回给 agent", async (t) => {
  const { conn } = connection(t, { FAKE_ACP_MODE: "permission" })
  await conn.start()
  await conn.initialize()
  const { sessionId } = await conn.sessionNew({ cwd: "/tmp" })
  const result = await conn.prompt({ sessionId, content: [{ type: "text", text: "跑个命令" }] })
  assert.equal(result.stopReason, "end_turn")
  assert.equal(result._meta?.chosen, "allow-once")
})

test("session/load 不认识的会话要报错，能识别的要成功", async (t) => {
  const { conn } = connection(t, { FAKE_ACP_KNOWN: "sess-known" })
  await conn.start()
  await conn.initialize()
  await assert.rejects(
    () => conn.sessionLoad({ sessionId: "sess-other", cwd: "/tmp" }),
    (error) => error instanceof AcpError && String(error.message).includes("unknown session"),
  )
  await conn.sessionLoad({ sessionId: "sess-known", cwd: "/tmp" })
})

test("协议版本不一致直接拒绝", async (t) => {
  const { conn } = connection(t, { FAKE_ACP_VERSION: "2" })
  await conn.start()
  await assert.rejects(
    () => conn.initialize(),
    (error) => error instanceof AcpError && error.kind === "unsupported_version",
  )
})

test("agent 中途退出：未完成的请求以 agent_exited 收场", async (t) => {
  const { conn } = connection(t, { FAKE_ACP_MODE: "crash" })
  await conn.start()
  await conn.initialize()
  const { sessionId } = await conn.sessionNew({ cwd: "/tmp" })
  await assert.rejects(
    () => conn.prompt({ sessionId, content: [{ type: "text", text: "炸" }] }),
    (error) => error instanceof AcpError && error.kind === "unreachable",
  )
})

test("session/cancel 让在跑的一轮以 cancelled 收场", async (t) => {
  const { conn } = connection(t, { FAKE_ACP_MODE: "slow" })
  await conn.start()
  await conn.initialize()
  const { sessionId } = await conn.sessionNew({ cwd: "/tmp" })
  const pending = conn.prompt({ sessionId, content: [{ type: "text", text: "慢慢来" }] })
  await new Promise((resolve) => setTimeout(resolve, 50))
  conn.cancel(sessionId)
  const result = await pending
  assert.equal(result.stopReason, "cancelled")
})

test("坏帧不拖垮连接", async (t) => {
  const { conn } = connection(t)
  await conn.start()
  // 直接往 stdout 里塞坏数据的是假 agent 自己；这里只验证真实帧之后仍然能通。
  await conn.initialize()
  const { sessionId } = await conn.sessionNew({ cwd: "/tmp" })
  const result = await conn.prompt({ sessionId, content: [{ type: "text", text: "还活着吗" }] })
  assert.equal(result.stopReason, "end_turn")
})
