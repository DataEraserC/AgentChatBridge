// 入口脚本软链启动回归。
//
// profile/current 是一个软链；从 `profile/current/harnesses/<h>/src/driver.mjs`
// 启动时，process.argv[1] 是软链路径，而 import.meta.url 是解析后的真实路径。
// 旧实现直接字符串比较，会把主模块误判成「被 import」，入口脚本静默 exit 0。
//
// 这里对三个 driver 各做两次启动：经临时 `profile/current` 软链，和直接跑真实
// 路径。断言两种方式都能进入 main 并打印就绪行。全部离线：假 token、不可达的
// bridge/gateway 端口，不依赖真实服务。

import test from "node:test"
import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"

import { isMainModule } from "../main-module.mjs"

const HARNESSES = path.resolve(import.meta.dirname, "../..")
const REPO_ROOT = path.resolve(HARNESSES, "..")

const DRIVERS = [
  {
    name: "dsh",
    script: path.join(HARNESSES, "dsh/src/driver.mjs"),
    ready: /dsh driver ready \(stdio\)/,
    env: () => ({ ACB_MCP_PORT: "0", ACB_HARNESS: "dsh-1" }),
  },
  {
    name: "workbuddy",
    script: path.join(HARNESSES, "workbuddy/src/driver.mjs"),
    ready: /WorkBuddy driver ready/,
    env: (tmp) => {
      const cli = path.join(tmp, "fake/codebuddy-headless.js")
      fs.mkdirSync(path.dirname(cli), { recursive: true })
      fs.writeFileSync(cli, "// fake headless CLI for entry-point test\n")
      return {
        ACB_MCP_PORT: "0",
        ACB_WB_CLI: cli,
        ACB_WB_STATE_DIR: path.join(tmp, "state/workbuddy"),
      }
    },
  },
  {
    name: "lobsterai",
    script: path.join(HARNESSES, "lobsterai/src/driver.mjs"),
    ready: /LobsterAI driver ready/,
    env: (tmp) => {
      const state = path.join(tmp, "state/lobsterai")
      fs.mkdirSync(state, { recursive: true })
      fs.writeFileSync(
        path.join(state, "openclaw.json"),
        JSON.stringify({
          models: {
            providers: {
              "lobsterai-server": {
                baseUrl: "http://127.0.0.1:9/v1/",
                apiKey: "proxy-managed",
              },
            },
          },
        }),
      )
      return {
        ACB_MCP_PORT: "0",
        ACB_LA_STATE_DIR: state,
        ACB_LA_GATEWAY_URL: "ws://127.0.0.1:9/",
        ACB_LA_GATEWAY_TOKEN: "dummy",
      }
    },
  },
]

function childEnv(tmp, extra) {
  const env = { ...process.env }
  for (const key of Object.keys(env)) {
    if (
      /^(ACB_|DSH_|CODEBUDDY_|WORKBUDDY_|LOBSTER_)/.test(key) ||
      key === "LANG" ||
      key === "LC_ALL" ||
      key === "LC_MESSAGES"
    ) {
      delete env[key]
    }
  }
  Object.assign(env, {
    HOME: path.join(tmp, "home"),
    XDG_STATE_HOME: path.join(tmp, "state"),
    XDG_CONFIG_HOME: path.join(tmp, "config"),
    // 固定英文日志，避免断言依赖宿主的 locale。
    LANG: "C",
    LC_ALL: "C",
    ACB_TOKEN: "dummy-token",
    ACB_ENDPOINT: "127.0.0.1:1",
  }, extra)
  return env
}

/** 启动入口脚本，等到就绪行后杀掉；提前退出或超时都算失败。 */
function launch(script, env, ready, timeoutMs = 10_000) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [script], {
      env,
      // stdin 保持 pipe：dsh 的 stdio MCP 模式要靠它保持进程存活。
      stdio: ["pipe", "pipe", "pipe"],
    })
    let output = ""
    let settled = false
    let timer
    const settle = (fn, value) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      try {
        child.kill("SIGKILL")
      } catch {
        // 已经退出时 kill 会抛，忽略。
      }
      fn(value)
    }
    timer = setTimeout(
      () => settle(reject, new Error(`timed out waiting for ${ready}; stderr:\n${output}`)),
      timeoutMs,
    )
    child.stderr.on("data", (chunk) => {
      output += chunk
      if (!settled && ready.test(output)) settle(resolve, output)
    })
    child.on("error", (error) => settle(reject, error))
    child.on("exit", (code, signal) => {
      if (!settled) {
        settle(
          reject,
          new Error(`exited before ready (code=${code}, signal=${signal}); stderr:\n${output}`),
        )
      }
    })
  })
}

test("isMainModule is symlink-safe and tolerates realpath errors", () => {
  assert.equal(isMainModule(import.meta.url, ["node", "/no/such/entry.mjs"]), false)
  assert.equal(isMainModule(import.meta.url, ["node"]), false)
})

for (const driver of DRIVERS) {
  test(`${driver.name}: starts via profile symlink and real path`, async (t) => {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), `acb-entry-${driver.name}-`))
    t.after(() => fs.rmSync(tmp, { recursive: true, force: true }))
    const env = childEnv(tmp, driver.env(tmp))

    // 模拟 profile/current -> 产物根目录；从软链路径启动。
    fs.mkdirSync(path.join(tmp, "profile"), { recursive: true })
    fs.symlinkSync(REPO_ROOT, path.join(tmp, "profile/current"))
    const viaLink = path.join(
      tmp,
      "profile/current/harnesses",
      driver.name,
      "src",
      path.basename(driver.script),
    )

    const symlinkOutput = await launch(viaLink, env, driver.ready)
    assert.match(symlinkOutput, driver.ready)

    const directOutput = await launch(driver.script, env, driver.ready)
    assert.match(directOutput, driver.ready)
  })
}
