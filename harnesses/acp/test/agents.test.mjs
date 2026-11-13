// 声明层的测试：认清「启动命令是数据，不是代码」。

import assert from "node:assert/strict"
import test from "node:test"

import {
  AcpConfigError,
  agentFromCommand,
  parseAgents,
  resolveAgent,
} from "../src/agents.mjs"

test("按声明原样保留 argv：不拆空格、不去前导横线", () => {
  const { agents, errors } = parseAgents(JSON.stringify({
    acme: {
      title: "Acme",
      processExecutables: ["acme-native"],
      commands: [{
        match: ["acme-run", "--profile", "work"],
        acp: ["/opt/bin/acme-bridge", "serve", "--stdio", "--label", "hello world", ""],
      }],
    },
  }))
  assert.deepEqual(errors, [])
  assert.equal(agents.length, 1)
  assert.deepEqual(agents[0].acp, [
    "/opt/bin/acme-bridge",
    "serve",
    "--stdio",
    "--label",
    "hello world",
    "",
  ])
})

test("坏条目跳过、好条目留下，并说明原因", () => {
  const { agents, errors } = parseAgents(JSON.stringify({
    good: { commands: [{ acp: ["codebuddy-acp", "--acp"] }] },
    "bad id!": { commands: [{ acp: ["x"] }] },
    nocommand: { commands: [] },
    notargv: { commands: [{ acp: "codebuddy-acp --acp" }] },
  }))
  assert.deepEqual(agents.map((agent) => agent.id), ["good"])
  assert.equal(errors.length, 3)
  assert.ok(errors.some((error) => error.includes("bad id!")))
  assert.ok(errors.some((error) => error.includes("nocommand")))
  assert.ok(errors.some((error) => error.includes("notargv")))
})

test("整份文档坏掉时不给半套：agents 为空", () => {
  assert.deepEqual(parseAgents("{").agents, [])
  assert.deepEqual(parseAgents("[]").agents, [])
  assert.deepEqual(parseAgents("").agents, [])
})

test("只有一个 agent 时不必再指定 id", () => {
  const agent = resolveAgent({
    ACB_ACP_AGENTS: JSON.stringify({ codebuddy: { commands: [{ acp: ["codebuddy-acp", "--acp"] }] } }),
  })
  assert.equal(agent.id, "codebuddy")
  assert.deepEqual(agent.acp, ["codebuddy-acp", "--acp"])
})

test("多个 agent 必须点名，点错了列出可选项", () => {
  const env = {
    ACB_ACP_AGENTS: JSON.stringify({
      a: { commands: [{ acp: ["a"] }] },
      b: { commands: [{ acp: ["b"] }] },
    }),
  }
  assert.throws(() => resolveAgent(env), AcpConfigError)
  assert.throws(() => resolveAgent({ ...env, ACB_ACP_AGENT: "c" }), /可用：a, b/)
  assert.equal(resolveAgent({ ...env, ACB_ACP_AGENT: "b" }).acp[0], "b")
})

test("没有声明时 ACB_ACP_COMMAND 就是唯一 agent", () => {
  const agent = resolveAgent({
    ACB_ACP_COMMAND: '["multi-instance","workbuddy","1","codebuddy-acp","--acp"]',
    ACB_HARNESS: "codebuddy",
  })
  assert.equal(agent.id, "codebuddy")
  assert.equal(agent.acp[0], "multi-instance")
  assert.equal(agent.acp.length, 5)
})

test("既没声明也没命令时报错，不猜默认值", () => {
  assert.throws(() => resolveAgent({}), /ACB_ACP_AGENTS/)
  assert.throws(() => agentFromCommand("codebuddy-acp --acp"), /JSON 数组/)
})
