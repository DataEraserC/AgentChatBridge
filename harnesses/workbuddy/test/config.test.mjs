// WorkBuddy 配置解析的单测：CLI 入口守卫、会话 id、默认预批准工具。
//
// 这些用例不需要真实 WorkBuddy 登录/额度。

import test from "node:test"
import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

import { assertHeadlessCli, readConfig, resolveCliPath, sessionIdFor } from "../src/config.mjs"

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

test("会话 id 归一化：非法字符替换、必须字母数字开头、可带代次", () => {
  assert.equal(sessionIdFor("main"), "acb-main")
  assert.equal(sessionIdFor("qq:group/123"), "acb-qq-group-123")
  assert.equal(sessionIdFor("0912"), "acb-0912")
  assert.match(sessionIdFor("main", "abc"), /^acb-main-rabc$/)
})

test("读完整配置：默认预批准裸名和 mcp__acb__ 前缀名", () => {
  const config = readConfig({
    ACB_WB_CLI: FAKE_CLI,
    ACB_TOKEN: "acb-token",
  })
  assert.equal(config.harness, "workbuddy")
  assert.equal(config.cliPath, FAKE_CLI)
  assert.equal(config.settings, '{"enableAllProjectMcpServers":true}')
  assert.equal(config.tools, null, "默认不传 --tools")
  assert.ok(config.allowedTools.includes("send_message"))
  assert.ok(config.allowedTools.includes("mcp__acb__send_message"))
})

test("ACB_WB_TOOLS / ACB_WB_ALLOWED_TOOLS 可覆盖默认", () => {
  const config = readConfig({
    ACB_WB_CLI: FAKE_CLI,
    ACB_TOKEN: "acb-token",
    ACB_WB_TOOLS: "default",
    ACB_WB_ALLOWED_TOOLS: "send_message, ack_messages",
    ACB_WB_PERMISSION_MODE: "acceptEdits",
  })
  assert.equal(config.tools, "default")
  assert.deepEqual(config.allowedTools, ["send_message", "ack_messages"])
  assert.equal(config.permissionMode, "acceptEdits")
})
