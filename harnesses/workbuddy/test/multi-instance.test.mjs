// WorkBuddy 多实例协议层单测：两个实例并行跑，MCP 端口、state 根、会话 id 前缀不互踩。
//
// 这里不启动真 CLI，只用假 headless CLI + 假桥；真登录后的隔离验证见 docs 的未验证清单。

import test from "node:test"
import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

import { BridgeLink } from "../../opencode/plugin/acb-wire.mjs"
import { FakeBridge, delivery } from "../../opencode/test/fake-bridge.mjs"
import { McpHttpServer } from "../../dsh/src/mcp-http-server.mjs"
import { callBridge } from "../../shared/bridge-tools.mjs"
import { CodebuddyClient } from "../src/codebuddy-client.mjs"
import { TurnDriver } from "../src/driver.mjs"

const FAKE_CLI = path.join(import.meta.dirname, "fake-codebuddy-headless.mjs")

async function connectedLink(bridge, harness) {
  const link = new BridgeLink({ endpoint: bridge.endpoint, token: "t-main", harness, log: () => {} })
  await link.connect()
  return link
}

function makeInstance(instance, stateDir) {
  const cli = new CodebuddyClient({
    cliPath: FAKE_CLI,
    log: () => {},
    home: stateDir,
    configDir: path.join(stateDir, ".codebuddy"),
    dataRoot: path.join(stateDir, "data"),
    baseEnv: {
      ...process.env,
      FAKE_CODEBUDDY_MODE: "ok",
      FAKE_CODEBUDDY_MARKER: path.join(stateDir, "marker.json"),
    },
  })
  const driver = new TurnDriver({ cli, instance, log: () => {} })
  return { cli, driver }
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

async function answerToolCall(bridge) {
  const frame = await nextFrameOfType(bridge, "tool_call")
  bridge.socket.write(`${JSON.stringify({
    frame: "tool_result",
    id: frame.id,
    result: { platform_id: `p-${frame.ticket}` },
  })}\n`)
  return frame
}

function readMarker(stateDir) {
  return JSON.parse(fs.readFileSync(path.join(stateDir, "marker.json"), "utf8"))
}

function mcpUrlFromArgs(args) {
  return JSON.parse(args[args.indexOf("--mcp-config") + 1]).mcpServers.acb.url
}

test("两个实例并行：MCP 端口、state 根、会话 id 前缀互不共享", async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "acb-wb-multi-"))
  const state1 = path.join(root, "workbuddy-1")
  const state2 = path.join(root, "workbuddy-2")
  fs.mkdirSync(state1, { recursive: true })
  fs.mkdirSync(state2, { recursive: true })

  const bridge1 = await new FakeBridge().start()
  const bridge2 = await new FakeBridge().start()
  const link1 = await connectedLink(bridge1, "workbuddy-1")
  const link2 = await connectedLink(bridge2, "workbuddy-2")
  const one = makeInstance("workbuddy-1", state1)
  const two = makeInstance("workbuddy-2", state2)
  const mcp1 = await startMcp(one.driver, link1)
  const mcp2 = await startMcp(two.driver, link2)
  one.driver.mcpUrl = mcp1.url
  two.driver.mcpUrl = mcp2.url

  t.after(async () => {
    link1.close()
    link2.close()
    mcp1.server.stop()
    mcp2.server.stop()
    await bridge1.stop()
    await bridge2.stop()
    fs.rmSync(root, { recursive: true, force: true })
  })

  // 两个回调端口必须不同：每个实例自己的 MCP server 监听自己的端口。
  assert.notEqual(one.driver.mcpUrl, two.driver.mcpUrl)
  assert.notEqual(mcp1.url, mcp2.url)

  link1.ticket = "wb1-1"
  link2.ticket = "wb2-1"
  const first1 = one.driver.run(delivery({ ticket: "wb1-1", session: "main" }), link1)
  const first2 = two.driver.run(delivery({ ticket: "wb2-1", session: "main" }), link2)

  // 两个假模型同时在各自的 MCP server 上调 send_message。
  const [tool1, tool2] = await Promise.all([
    answerToolCall(bridge1),
    answerToolCall(bridge2),
  ])
  assert.equal(tool1.ticket, "wb1-1")
  assert.equal(tool2.ticket, "wb2-1")

  const [end1, end2] = await Promise.all([
    nextFrameOfType(bridge1, "turn_end"),
    nextFrameOfType(bridge2, "turn_end"),
  ])
  await Promise.all([first1, first2])

  assert.equal(end1.ticket, "wb1-1")
  assert.equal(end2.ticket, "wb2-1")
  assert.equal(end1.error, undefined)
  assert.equal(end2.error, undefined)

  // 会话 id 前缀隔离。
  assert.equal(one.cli.runs[0].sessionId, "acb-workbuddy-1-main")
  assert.equal(two.cli.runs[0].sessionId, "acb-workbuddy-2-main")
  assert.notEqual(one.cli.runs[0].sessionId, two.cli.runs[0].sessionId)

  // MCP URL 各用各的端口。
  assert.equal(mcpUrlFromArgs(one.cli.runs[0].args), mcp1.url)
  assert.equal(mcpUrlFromArgs(two.cli.runs[0].args), mcp2.url)

  // HOME / CODEBUDDY_CONFIG_DIR 指向各自 state，登录态与会话记录不共享。
  const marker1 = readMarker(state1)
  const marker2 = readMarker(state2)
  assert.equal(marker1.home, state1)
  assert.equal(marker2.home, state2)
  assert.equal(marker1.configDir, path.join(state1, ".codebuddy"))
  assert.equal(marker2.configDir, path.join(state2, ".codebuddy"))
  assert.notEqual(marker1.configDir, marker2.configDir)
  assert.equal(marker1.sessionId, "acb-workbuddy-1-main")
  assert.equal(marker2.sessionId, "acb-workbuddy-2-main")

  // 第二轮：两个实例都用 -r 续自己的会话，不串到对方。
  link1.ticket = "wb1-2"
  link2.ticket = "wb2-2"
  const second1 = one.driver.run(delivery({ ticket: "wb1-2", session: "main" }), link1)
  const second2 = two.driver.run(delivery({ ticket: "wb2-2", session: "main" }), link2)
  await Promise.all([answerToolCall(bridge1), answerToolCall(bridge2)])
  const [end3, end4] = await Promise.all([
    nextFrameOfType(bridge1, "turn_end"),
    nextFrameOfType(bridge2, "turn_end"),
  ])
  await Promise.all([second1, second2])

  assert.equal(end3.error, undefined)
  assert.equal(end4.error, undefined)
  assert.equal(one.cli.runs[1].resume, true)
  assert.equal(two.cli.runs[1].resume, true)
  assert.equal(one.cli.runs[1].sessionId, "acb-workbuddy-1-main")
  assert.equal(two.cli.runs[1].sessionId, "acb-workbuddy-2-main")

  const marker1b = readMarker(state1)
  const marker2b = readMarker(state2)
  assert.equal(marker1b.resume, true)
  assert.equal(marker2b.resume, true)
  assert.equal(marker1b.sessionId, "acb-workbuddy-1-main")
  assert.equal(marker2b.sessionId, "acb-workbuddy-2-main")
})
