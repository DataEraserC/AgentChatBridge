// WorkBuddy 配置解析的单测：CLI 入口守卫、会话 id、默认预批准工具。
//
// 这些用例不需要真实 WorkBuddy 登录/额度。

import test from "node:test"
import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

import {
  assertHeadlessCli,
  defaultStateDir,
  readConfig,
  resolveCliPath,
  sanitizeInstance,
  sessionIdFor,
} from "../src/config.mjs"

const FAKE_CLI = path.join(import.meta.dirname, "fake-codebuddy-headless.mjs")

test("显式 headless CLI 直接放行", () => {
  assert.equal(assertHeadlessCli(FAKE_CLI, {}), FAKE_CLI)
})

test("拒绝 bin/codebuddy（未强制无头）", () => {
  const bin = "/run/current-system/sw/bin/codebuddy"
  assert.throws(() => assertHeadlessCli(bin, {}), /not cli\/dist\/codebuddy-headless\.js|headless/i)
  assert.equal(
    assertHeadlessCli(bin, { CODEBUDDY_FORCE_HEADLESS_BUNDLE: "1" }),
    bin,
  )
})

test("拒绝 GUI workbuddy wrapper", () => {
  assert.throws(
    () => assertHeadlessCli("/run/current-system/sw/bin/workbuddy", { CODEBUDDY_FORCE_HEADLESS_BUNDLE: "1" }),
    /headless/i,
  )
})

test("ACB_WB_CLI 指向不存在的文件时明确报错", () => {
  assert.throws(
    () => resolveCliPath({ ACB_WB_CLI: "/tmp/definitely-not-a-cli.js" }),
    /cannot find the WorkBuddy headless CLI/i,
  )
})

test("WORKBUDDY_RESOURCES 能推导出 app.asar.unpacked 里的 headless bundle", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "acb-wb-res-"))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const dist = path.join(root, "app.asar.unpacked", "cli", "dist")
  fs.mkdirSync(dist, { recursive: true })
  const target = path.join(dist, "codebuddy-headless.js")
  fs.writeFileSync(target, "#!/usr/bin/env node\n")

  assert.equal(
    resolveCliPath({ WORKBUDDY_RESOURCES: root, PATH: "" }),
    target,
  )
})

test("会话 id 前缀带实例名，跨实例不串；可带代次", () => {
  assert.equal(sessionIdFor("main", "workbuddy-1"), "acb-workbuddy-1-main")
  assert.equal(sessionIdFor("main", "workbuddy-2"), "acb-workbuddy-2-main")
  assert.equal(sessionIdFor("qq:group/123", "workbuddy-1"), "acb-workbuddy-1-qq-group-123")
  assert.equal(sessionIdFor("0912", "workbuddy-1"), "acb-workbuddy-1-0912")
  assert.match(sessionIdFor("main", "workbuddy-1", "abc"), /^acb-workbuddy-1-main-rabc$/)
})

test("实例名归一化；默认 state 目录带实例名", () => {
  assert.equal(sanitizeInstance("work buddy/1"), "work-buddy-1")
  assert.equal(sanitizeInstance(""), "workbuddy-1")
  assert.equal(
    defaultStateDir("workbuddy-1", { XDG_STATE_HOME: "/tmp/acb-state" }),
    "/tmp/acb-state/acb/workbuddy/workbuddy-1",
  )
  assert.equal(
    defaultStateDir("workbuddy-2", { XDG_STATE_HOME: "/tmp/acb-state" }),
    "/tmp/acb-state/acb/workbuddy/workbuddy-2",
  )
})

test("读完整配置：默认实例、state 根、裸名 + mcp__acb__ 前缀名", () => {
  const config = readConfig({
    ACB_WB_CLI: FAKE_CLI,
    ACB_TOKEN: "acb-token",
    XDG_STATE_HOME: "/tmp/acb-wb-state",
  })
  assert.equal(config.instance, "workbuddy-1")
  assert.equal(config.harness, "workbuddy-1")
  assert.equal(config.stateDir, "/tmp/acb-wb-state/acb/workbuddy/workbuddy-1")
  assert.equal(config.home, config.stateDir)
  assert.equal(config.configDir, path.join(config.stateDir, ".codebuddy"))
  assert.equal(config.dataRoot, path.join(config.stateDir, "data"))
  assert.equal(config.mcpPort, 0)
  assert.equal(config.cliPath, FAKE_CLI)
  assert.equal(config.settings, '{"enableAllProjectMcpServers":true}')
  assert.equal(config.tools, null, "默认不传 --tools")
  assert.ok(config.allowedTools.includes("send_message"))
  assert.ok(config.allowedTools.includes("mcp__acb__send_message"))
})

test("ACB_WB_TOOLS / ALLOWED_TOOLS / MCP 端口可覆盖默认", () => {
  const config = readConfig({
    ACB_WB_CLI: FAKE_CLI,
    ACB_TOKEN: "acb-token",
    ACB_WB_INSTANCE: "workbuddy-2",
    ACB_HARNESS: "workbuddy-2",
    ACB_WB_STATE_DIR: "/tmp/acb-wb-instance-2",
    ACB_MCP_PORT: "7719",
    ACB_WB_TOOLS: "default",
    ACB_WB_ALLOWED_TOOLS: "send_message, ack_messages",
    ACB_WB_PERMISSION_MODE: "acceptEdits",
  })
  assert.equal(config.instance, "workbuddy-2")
  assert.equal(config.harness, "workbuddy-2")
  assert.equal(config.stateDir, "/tmp/acb-wb-instance-2")
  assert.equal(config.mcpPort, 7719)
  assert.equal(config.tools, "default")
  assert.deepEqual(config.allowedTools, [
    "send_message",
    "mcp__acb__send_message",
    "ack_messages",
    "mcp__acb__ack_messages",
  ])
  assert.equal(config.permissionMode, "acceptEdits")
})

test("ACB_HARNESS 与 ACB_WB_INSTANCE 不一致时拒绝", () => {
  assert.throws(
    () => readConfig({
      ACB_WB_CLI: FAKE_CLI,
      ACB_WB_INSTANCE: "workbuddy-1",
      ACB_HARNESS: "workbuddy-2",
    }),
    /must match|串行键/i,
  )
})

test("ACB_MCP_PORT 非法值明确报错", () => {
  assert.throws(
    () => readConfig({
      ACB_WB_CLI: FAKE_CLI,
      ACB_MCP_PORT: "abc",
    }),
    /ACB_MCP_PORT must be an integer/i,
  )
})
