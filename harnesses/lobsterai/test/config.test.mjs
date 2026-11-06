// 配置解析的单测：state 目录、gateway 地址/token、LobsterAI 额度链路校验。
//
// 这些用例全是本地临时目录，不碰真实 LobsterAI 状态，也不需要登录/额度。

import test from "node:test"
import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

import { readConfig, resolveStateDir } from "../src/config.mjs"

function makeState({
  quota = true,
  port = 18789,
  token = "gw-token",
  writePort = true,
  writeToken = true,
  writeConfig = true,
} = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "acb-la-"))
  const stateDir = path.join(root, "openclaw", "state")
  fs.mkdirSync(stateDir, { recursive: true })

  if (writeConfig) {
    const config = quota
      ? {
          models: {
            providers: {
              "lobsterai-server": {
                baseUrl: "http://127.0.0.1:45573/v1",
                apiKey: "proxy-managed",
              },
            },
          },
        }
      : {
          models: {
            providers: {
              openai: { baseUrl: "https://api.openai.com/v1", apiKey: "sk-test" },
            },
          },
        }
    fs.writeFileSync(path.join(stateDir, "openclaw.json"), JSON.stringify(config, null, 2))
  }
  if (writePort) {
    fs.writeFileSync(path.join(stateDir, "gateway-port.json"), JSON.stringify({ port, updatedAt: 1 }))
  }
  if (writeToken) {
    fs.writeFileSync(path.join(stateDir, "gateway-token"), `${token}\n`)
  }

  return {
    root,
    stateDir,
    cleanup() {
      fs.rmSync(root, { recursive: true, force: true })
    },
    env(extra = {}) {
      return {
        ACB_LA_STATE_DIR: stateDir,
        ACB_TOKEN: "acb-token",
        ...extra,
      }
    },
  }
}

test("读完整配置：state 目录 + gateway-port/token + lobsterai-server", (t) => {
  const fixture = makeState()
  t.after(() => fixture.cleanup())

  const config = readConfig(fixture.env())
  assert.equal(config.stateDir, fixture.stateDir)
  assert.equal(config.gatewayUrl, "ws://127.0.0.1:18789")
  assert.equal(config.gatewayToken, "gw-token")
  assert.equal(config.quotaProvider, "lobsterai-server")
  assert.equal(config.harness, "lobsterai")
  assert.equal(config.endpoint, "127.0.0.1:7700")
})

test("state 目录不存在：明确报错，不猜默认路径", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "acb-la-missing-"))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const missing = path.join(root, "nope", "openclaw", "state")

  assert.throws(
    () => readConfig({ ACB_LA_STATE_DIR: missing, ACB_TOKEN: "x" }),
    /state directory does not exist/i,
  )
})

test("拒绝裸 OpenClaw 的 ~/.openclaw", () => {
  const bare = path.join(os.homedir(), ".openclaw")
  assert.throws(
    () => readConfig({ ACB_LA_STATE_DIR: bare, ACB_TOKEN: "x" }),
    /bare OpenClaw|~\/\.openclaw/i,
  )
})

test("openclaw.json 不是 LobsterAI 生成：quota 链路校验失败", (t) => {
  const fixture = makeState({ quota: false })
  t.after(() => fixture.cleanup())

  assert.throws(
    () => readConfig(fixture.env()),
    /lobsterai-server|token proxy|quota/i,
  )
})

test("缺 gateway-port.json：明确报错", (t) => {
  const fixture = makeState({ writePort: false })
  t.after(() => fixture.cleanup())

  assert.throws(
    () => readConfig(fixture.env()),
    /gateway-port\.json|gateway port/i,
  )
})

test("缺 gateway-token：明确报错", (t) => {
  const fixture = makeState({ writeToken: false })
  t.after(() => fixture.cleanup())

  assert.throws(
    () => readConfig(fixture.env()),
    /gateway-token|gateway token/i,
  )
})

test("显式 gateway 地址与 token 覆盖文件", (t) => {
  const fixture = makeState({ writePort: false, writeToken: false })
  t.after(() => fixture.cleanup())

  const config = readConfig(fixture.env({
    ACB_LA_GATEWAY_URL: "ws://127.0.0.1:19001",
    ACB_LA_GATEWAY_TOKEN: "explicit-token",
  }))
  assert.equal(config.gatewayUrl, "ws://127.0.0.1:19001/")
  assert.equal(config.gatewayToken, "explicit-token")
})

test("ACB_LA_USER_DATA 推导出 <userData>/openclaw/state", (t) => {
  const fixture = makeState()
  t.after(() => fixture.cleanup())

  const env = {
    ACB_LA_USER_DATA: path.dirname(path.dirname(fixture.stateDir)),
    ACB_TOKEN: "acb-token",
  }
  assert.equal(
    resolveStateDir(env),
    fixture.stateDir,
  )
})

test("ACB_TOKEN 缺失由 driver 决定退出；配置本身不替它编默认值", (t) => {
  const fixture = makeState()
  t.after(() => fixture.cleanup())

  const config = readConfig({ ACB_LA_STATE_DIR: fixture.stateDir })
  assert.equal(config.token, "")
})
