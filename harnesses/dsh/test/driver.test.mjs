// 驱动器逻辑的单测：假桥 + 假 dsh，验「turn_start → prompt → 等 turn/end → turn_end」
// 闭环，以及 MCP 工具调用转成带票的 tool_call。
//
// 假桥直接复用 opencode 的 fake-bridge（同一个协议，不重复造）。

import test from "node:test"
import assert from "node:assert/strict"

import { BridgeLink } from "../../opencode/plugin/acb-wire.mjs"
import { DshError } from "../src/dsh-client.mjs"
import { TurnDriver, callBridge } from "../src/driver.mjs"
import { RESIDENT_TOOL_NAMES } from "../../shared/tools.mjs"
import { FakeBridge, TICKET, delivery } from "../../opencode/test/fake-bridge.mjs"

// 可控制 follow 的假 dsh。
class ControllableDsh {
  constructor() {
    this.prompts = []
    this.sessions = []
    this.modelPicks = []
    this.lastFollow = null
    this.createCalls = []
    this.promptErrors = []
    this.workspaceCalls = []
    // 设了就按它匹配请求的 sessionId 抛 conflict（测驱动的冲突兜底）。
    this.conflictOn = null
    this.workspaceCreated = false
  }

  async createSession(args = {}) {
    this.createCalls.push(args)
    if (this.conflictOn && this.conflictOn.test(String(args.sessionId ?? ""))) {
      throw new Error('session/create 失败（session/conflict）：session "x" belongs to "/a", not "/b"')
    }
    const id = `session-${this.sessions.length + 1}`
    this.sessions.push(id)
    return id
  }

  async ensureWorkspaceTitle(path, title) {
    const created = this.workspaceCreated
    this.workspaceCalls.push({ path, title, created })
    return { workspaceId: "ws-1", created }
  }

  async selectModel(sessionId, choice) {
    this.modelPicks.push({ sessionId, ...choice })
    return { selected: choice }
  }

  async prompt(sessionId, content) {
    if (this.promptErrors.length) throw new Error(this.promptErrors.shift())
    this.prompts.push({ sessionId, content })
    return { accepted: true }
  }

  async follow(sessionId) {
    const follow = makeManualStream()
    this.lastFollow = follow
    return follow
  }
}

function makeManualStream() {
  const queue = []
  let wake
  let ended = false
  return {
    async *[Symbol.asyncIterator]() {
      while (true) {
        while (queue.length > 0) {
          const item = queue.shift()
          if (item === "end") return
          yield item
        }
        if (ended) return
        await new Promise((resolve) => {
          wake = resolve
        })
      }
    },
    push(item) {
      queue.push(item)
      wake?.()
      wake = undefined
    },
    end() {
      ended = true
      wake?.()
      wake = undefined
    },
  }
}

/** 起一个假桥 + 连上 + 握手的 link。FakeBridge 的 #greet 在 connect 时自动回 ready。 */
async function connectedLink(bridge, token = "t-main") {
  const link = new BridgeLink({ endpoint: bridge.endpoint, token, harness: "dsh", log: () => {} })
  await link.connect()
  return link
}

async function waitFor(predicate, message) {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}

/** 取下一帧，但跳过 report_map 的 control_result（建会话时 driver 会先回报映射）。 */
async function nextTurnEnd(bridge) {
  for (;;) {
    const frame = await bridge.next()
    if (frame.frame === "turn_end") return frame
    // 不是 turn_end（比如 report_map 的 control_result）就接着取下一帧。
  }
}

test("轮次驱动：turn_start → prompt → 等 turn/end → turn_end", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(delivery(), link)

  // 等 prompt 被调。
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  assert.equal(dsh.prompts[0].sessionId, "session-1")
  assert.ok(dsh.prompts[0].content[0].text.includes("在吗"), "投递内容应包含消息")

  // 推一个 turn/end。
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  // 应该有一个 turn_end 帧（跳过建会话时回报映射的 control_result）。
  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
})

test("首轮前 internal 补查 issue 未读：注入一行且不计 turnCalls", async (t) => {
  const bridge = await new FakeBridge({
    issueUnread: {
      count: 2,
      event_ids: ["ev_1", "ev_2"],
      events: [
        { event_id: "ev_1", issue_id: "is_1", title: "登录失败", kind: "issue.created", created_at: 1 },
        { event_id: "ev_2", issue_id: "is_2", title: "导出超时", kind: "issue.status_changed", created_at: 2 },
      ],
    },
  }).start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  // 这个测试绕过 wire 直接驱动一轮，但内部补查要带当前轮次的票。
  link.ticket = TICKET
  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")

  const text = dsh.prompts[0].content[0].text
  assert.match(text, /\[系统事件 issue_unread\] 2 条未读/)
  assert.match(text, /is_1 登录失败/)
  assert.equal(link.turnCalls, 0, "内部补查不计入 turnCalls")
  assert.equal(bridge.issueCalls.length, 2, "一次读取 + 一次 ack")
  assert.equal(bridge.issueCalls[0].args.arguments.subscriber, "session:main")

  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise
  await nextTurnEnd(bridge)

  // 同一会话第二轮：不再补查，也不再注入。
  link.ticket = "main-2"
  const second = driver.run(delivery({ ticket: "main-2" }), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")
  assert.equal(bridge.issueCalls.length, 2, "同一会话不再补查")
  assert.doesNotMatch(dsh.prompts[1].content[0].text, /issue_unread/)
  assert.equal(link.turnCalls, 0, "内部补查仍不计入 turnCalls")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
  await nextTurnEnd(bridge)
})

test("turn/end 后回 turn_end 帧（无错误）", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(delivery(), link)

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.equal(turnEnd.error, undefined)
})

test("seedMaps 复用桥下发的旧会话映射，不再 createSession", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  // 模拟桥 Ready 帧带回来的已落盘映射。delivery 的 session 是 "main"（见 fake-bridge）。
  driver.seedMaps([{ session: "main", harness_session: "session-acb-main-old" }])

  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")

  // 关键断言：用的是 seed 的旧会话，dsh 没有 createSession。
  assert.equal(dsh.prompts[0].sessionId, "session-acb-main-old", "应复用 seed 的会话 id")
  assert.equal(dsh.sessions.length, 0, "不应调用 createSession")

  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise
})

test("一轮失败后下次投递复用同一会话，不新建", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })

  // 第一轮：推一个带 error reason 的 turn/end，让它失败。
  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "第一轮 prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: { turn: "t1", reason: { kind: "error", error: { message: "炸了" } } },
  })
  await first

  // 第二轮：同一会话再投一次。
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")

  // 关键断言：两次 prompt 用的是同一个 sessionId，dsh 只 create 了一次。
  assert.equal(dsh.prompts[0].sessionId, dsh.prompts[1].sessionId, "失败后应复用同一会话")
  assert.equal(dsh.sessions.length, 1, "只应 createSession 一次")

  // 收尾：第二轮正常结束。
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
})

test("reset 之后换带代次的新 id，冲突时再兜底换新", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {}, cwd: "/w", preset: "p" })

  // 第一轮：正常建会话。
  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "第一轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await first
  assert.equal(dsh.createCalls[0].sessionId, "session-acb-main")
  assert.equal(dsh.createCalls[0].workspaceId, "ws-1", "workspace id 要传给 dsh（归组）")
  assert.equal(dsh.createCalls[0].agentPreset, "p", "配置了 preset 时原样传给 dsh")

  // reset：映射丢掉 + 代次 +1。下一轮必须用新 id，不能撞旧会话。
  driver.resetSession("main")
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
  assert.match(dsh.createCalls[1].sessionId, /^session-acb-main-r\w+$/, "reset 后 id 要带代次后缀")

  // 再 reset 一次并让 dsh 对固定 id 报 conflict：驱动要兜底换新 id 而不是硬顶。
  driver.resetSession("main")
  dsh.conflictOn = /^session-acb-main$/
  const third = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 3, "第三轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t3" } })
  await third
  assert.match(dsh.createCalls[2].sessionId, /^session-acb-main-r\w+$/, "第三次请求本身就该带新代次")
})

test("preset 始终显式传给 dsh：ACB_PRESET 缺失时回退到 acb-<session>", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {}, cwd: "/w", preset: "" })
  const run = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  assert.equal(
    dsh.createCalls[0].agentPreset,
    "acb-main",
    "不能把预设留给 profile 的 selectedDefault 兜底"
  )
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await run
})

test("旧 sessionId 失效时重建会话并重试", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {}, cwd: "/w" })
  dsh.promptErrors.push('session "session-1" not found')
  const run = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "重试后的 prompt 没被调")
  assert.equal(dsh.createCalls.length, 2, "应当重建一次会话")
  assert.notEqual(dsh.prompts[0].sessionId, "session-1", "重试用的是新会话")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await run
})

test("workspace 标题：只在新建 workspace 时命名，已有的不动", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  dsh.workspaceCreated = true
  const driver = new TurnDriver({ dsh, log: () => {}, cwd: "/w", workspaceTitle: "QQ 聊天" })
  const run = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await run
  assert.deepEqual(dsh.workspaceCalls, [{ path: "/w", title: "QQ 聊天", created: true }])

  // workspace 已存在（created:false）：第二次建会话可以再问一次 workspace，
  // 但只有 created 的那一次才该命名。
  dsh.workspaceCreated = false
  driver.resetSession("main")
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
  assert.equal(
    dsh.workspaceCalls.filter((call) => call.created).length,
    1,
    "已有 workspace 不重复命名"
  )
})

test("callBridge 把工具参数转成带票的 tool_call", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 手动设一个 ticket（模拟轮次中）。
  link.ticket = TICKET

  const callPromise = callBridge(link, "send_message", { text: "收到" })

  // 桥那边应该收到一个 tool_call 帧。
  const frame = await bridge.next()
  assert.equal(frame.frame, "tool_call")
  assert.equal(frame.ticket, TICKET)
  assert.equal(frame.tool, "send_message")
  assert.deepEqual(frame.args, { elements: [{ type: "text", text: "收到" }] })

  // 回一个 tool_result。
  bridge.socket.write(JSON.stringify({ frame: "tool_result", id: frame.id, result: { platform_id: "123" } }) + "\n")
  const result = await callPromise
  assert.ok(result.includes("123"))
})

test("没有 ticket 时 callBridge 报 unauthorized", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 不设 ticket，直接调。
  await assert.rejects(
    () => callBridge(link, "send_message", { text: "收到" }),
    /no turn is in progress|cannot be called/,
  )
})

// 为什么要有这条：dsh 把「上次用的模型」记在会话里，复用老会话时它优先于默认模型配置。
// 只改配置、不调 selectModel，老会话会悄悄继续打旧渠道（我们踩过一次：网关日志里还是旧模型）。
test("配了模型就给会话钉一次，同一会话不重复钉", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({
    dsh,
    log: () => {},
    model: { provider: "trae", model: "deepseek-v4.1-flash", reasoningEffort: "high" },
  })

  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await first

  assert.equal(dsh.modelPicks.length, 1, "第一轮应该钉一次模型")
  assert.deepEqual(dsh.modelPicks[0], {
    sessionId: "session-1",
    provider: "trae",
    model: "deepseek-v4.1-flash",
    reasoningEffort: "high",
  })

  // 第二轮：同一会话，不该再钉一次。
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
  assert.equal(dsh.modelPicks.length, 1, "同一会话只该钉一次")
})

test("没配模型就不碰会话的模型选择", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 只给 provider 不给 model：算没配全，不干预。
  const driver = new TurnDriver({ dsh, log: () => {}, model: { provider: "trae", model: "" } })
  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  assert.equal(dsh.modelPicks.length, 0, "没配模型就不该调 selectModel")
})

test("会话模型优先，selectModel 失败让这一轮失败", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  dsh.selectModel = async () => {
    throw new DshError("session/model-unavailable", "provider not found")
  }
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({
    dsh,
    log: () => {},
    model: { provider: "trae", model: "deepseek-v4.1-flash" },
  })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "new-api-y9000p2025-openai-responses/1Agent2Api/glm-5.3-flash",
      },
    }),
    link,
  )
  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.frame, "turn_end")
  assert.match(turnEnd.error, /unavailable/, "模型不存在要让整轮失败")
  assert.deepEqual(
    turnEnd.failure,
    { component: "model", class: "model_unconfigured" },
    "selectModel 对照 catalog 失败要标成模型未配置",
  )
  assert.equal(dsh.prompts.length, 0, "模型没选上就不该投话")
  await runPromise
})

test("dsh 不可达不能被报成模型不存在", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  dsh.selectModel = async () => {
    throw new DshError("unreachable", "cannot reach dsh at http://127.0.0.1:1: ECONNREFUSED")
  }
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({
    dsh,
    log: () => {},
    model: { provider: "trae", model: "deepseek-v4.1-flash" },
  })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "new-api-y9000p2025-openai-responses/1Agent2Api/glm-5.3-flash",
      },
    }),
    link,
  )
  const turnEnd = await nextTurnEnd(bridge)
  assert.match(turnEnd.error, /cannot reach dsh/, "transport 失败要保留 unreachable 文案")
  assert.doesNotMatch(turnEnd.error, /会话模型/, "不能把 transport 失败说成模型不存在")
  assert.deepEqual(
    turnEnd.failure,
    { component: "backend", class: "unreachable" },
    "dsh 不可达归到后端，不归到模型",
  )
  assert.equal(dsh.prompts.length, 0, "模型没选上就不该投话")
  await runPromise
})

test("会话模型按 provider/model 拆，模型 id 里的斜杠保留", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "new-api-y9000p2025-openai-responses/1Agent2Api/glm-5.3-flash",
      },
    }),
    link,
  )
  await waitFor(() => dsh.modelPicks.length === 1, "应调用 selectModel")
  assert.deepEqual(dsh.modelPicks[0], {
    sessionId: "session-1",
    provider: "new-api-y9000p2025-openai-responses",
    model: "1Agent2Api/glm-5.3-flash",
    reasoningEffort: "",
  })
  await waitFor(() => dsh.prompts.length === 1, "选好模型后要投话")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise
  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.error, undefined)
})

test("投递溢出时临时注入 get_history/fetch_records，普通轮次不注入", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  link.ticket = TICKET

  // 普通轮次：回看工具不在工具面里。
  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "普通轮次要投话")
  const normal = driver.toolDefs().map((tool) => tool.name)
  assert.equal(normal.includes("get_history"), false, "普通轮次不注入 get_history")
  assert.equal(normal.includes("fetch_records"), false, "普通轮次不注入 fetch_records")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await first

  // 投递溢出：本轮临时注入；下一轮没有溢出时收回。
  link.ticket = TICKET
  const overflow = driver.run(delivery({ more: 3 }), link)
  await waitFor(() => dsh.prompts.length === 2, "溢出轮次要投话")
  const injected = driver.toolDefs().map((tool) => tool.name)
  assert.ok(injected.includes("get_history"), "溢出时注入 get_history")
  assert.ok(injected.includes("fetch_records"), "溢出时注入 fetch_records")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await overflow

  link.ticket = TICKET
  const third = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 3, "收回轮次要投话")
  const restored = driver.toolDefs().map((tool) => tool.name)
  assert.equal(restored.includes("get_history"), false, "下一轮收回 get_history")
  assert.equal(restored.includes("fetch_records"), false, "下一轮收回 fetch_records")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t3" } })
  await third
})

test("surface_failure_falls_back_to_resident_only：拉取失败时退回常驻层，不退回全量", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })

  // 这个用例直接调 driver.run 而不走 turn_start 帧，手动补上桥保证的「轮内」状态；
  // 否则 BridgeLink 会以 no_active_turn 拒绝 surface 查询，测不到成功路径。
  link.ticket = TICKET

  // 第一轮：surface 成功，长尾工具进可见面。
  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "surface 之后要投话")
  const expanded = driver.toolDefs().map((tool) => tool.name)
  assert.ok(expanded.includes("notes"), "成功拉取后长尾工具应在工具面里")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await first

  // 第二轮：桥拒绝 surface，必须整体退回常驻白名单，而不是保留长尾或全量。
  bridge.failSurface = true
  link.ticket = TICKET
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "失败后仍要投话")
  assert.equal(driver.surfaceStatus, "surface_fallback")
  const fallback = driver.toolDefs().map((tool) => tool.name)
  assert.deepEqual(
    new Set(fallback),
    new Set(RESIDENT_TOOL_NAMES),
    "surface_fallback 只能留下常驻白名单",
  )
  assert.equal(fallback.includes("memory_git"), false, "不能退回长尾工具")
  assert.equal(fallback.length, RESIDENT_TOOL_NAMES.length)
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
})

// ---------------------------------------------------------------------------
// 后备模型链：主模型被拒就换下一个，把这一轮重放一遍
// ---------------------------------------------------------------------------

test("内容审核拒绝时换后备模型重试这一轮", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "trae/primary",
        model_fallbacks: ["trae/backup"],
      },
    }),
    link,
  )

  // 第一个模型：上游 router 回 400 content-blocked
  await waitFor(() => dsh.prompts.length === 1, "主模型 prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: {
      turn: "t1",
      reason: {
        kind: "error",
        error: { message: "content-blocked", code: "content-blocked" },
      },
    },
  })

  // 驱动应当换模型、把同一轮再投一次
  await waitFor(() => dsh.prompts.length === 2, "没有换后备模型重试")
  assert.deepEqual(
    dsh.modelPicks.map((pick) => pick.model),
    ["primary", "backup"],
    "先钉主模型，再钉后备模型",
  )
  assert.deepEqual(
    dsh.prompts[1].content,
    dsh.prompts[0].content,
    "重放的是同一段 prompt，不是新内容",
  )

  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.error, undefined, "后备模型跑完就不该再报失败")
  await runPromise
})

test("已经调过桥工具的轮次不换模型重放（避免重复发言）", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "trae/primary",
        model_fallbacks: ["trae/backup"],
      },
    }),
    link,
  )

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  // 模拟模型已经调过一次桥工具（可能已经把话发出去了）
  driver.noteToolCall()
  dsh.lastFollow.push({
    type: "turn/end",
    data: {
      turn: "t1",
      reason: { kind: "error", error: { message: "content-blocked", code: "content-blocked" } },
    },
  })

  const turnEnd = await nextTurnEnd(bridge)
  assert.match(turnEnd.error ?? "", /content-blocked/, "这一轮要如实报失败")
  assert.equal(dsh.prompts.length, 1, "说过话就不能重放，哪怕有后备模型")
  await runPromise
})

test("没有后备链时不重试，失败照常上报", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: { id: "main", name: "main", harness: "dsh", model: "trae/primary" },
    }),
    link,
  )

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: {
      turn: "t1",
      reason: { kind: "error", error: { message: "content-blocked", code: "content-blocked" } },
    },
  })

  const turnEnd = await nextTurnEnd(bridge)
  assert.match(turnEnd.error ?? "", /content-blocked/)
  assert.deepEqual(
    turnEnd.failure,
    { component: "model", class: "content_blocked" },
    "内容审核要标成模型侧失败，不能标成后端不可达",
  )
  assert.equal(dsh.prompts.length, 1, "没有后备链就不该重试")
  await runPromise
})

test("后备链里与主模型重复的项不重复尝试", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "trae/primary",
        // 第一项等于主模型：去重后只剩 backup
        model_fallbacks: ["trae/primary", "trae/backup"],
      },
    }),
    link,
  )

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: {
      turn: "t1",
      reason: { kind: "error", error: { message: "content-blocked", code: "content-blocked" } },
    },
  })
  await waitFor(() => dsh.prompts.length === 2, "应当换 backup 重试")
  assert.deepEqual(dsh.modelPicks.map((pick) => pick.model), ["primary", "backup"])
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await nextTurnEnd(bridge)
  await runPromise
})

test("后备模型不在 catalog 里时跳到再下一个", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const realSelect = dsh.selectModel.bind(dsh)
  // 记下所有尝试过的模型：钉失败的那次不会进 modelPicks（异常在 push 之前抛出）。
  const tried = []
  dsh.selectModel = async (sessionId, choice) => {
    tried.push(`${choice.provider}/${choice.model}`)
    if (choice.provider === "missing") {
      throw new DshError("session/model-unavailable", "provider not found")
    }
    return realSelect(sessionId, choice)
  }
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(
    delivery({
      session_info: {
        id: "main",
        name: "main",
        harness: "dsh",
        model: "trae/primary",
        // 第一项是写错的模型：钉不上就该跳过，不能把后面能用的也废掉
        model_fallbacks: ["missing/x", "trae/backup"],
      },
    }),
    link,
  )

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: {
      turn: "t1",
      reason: { kind: "error", error: { message: "content-blocked", code: "content-blocked" } },
    },
  })

  await waitFor(() => dsh.prompts.length === 2, "应当跳过写错的模型，用 backup 重试")
  assert.deepEqual(
    tried,
    ["trae/primary", "missing/x", "trae/backup"],
    "先是主模型，再试写错的（失败），最后落到 backup",
  )
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.error, undefined, "backup 跑完就不该报失败")
  await runPromise
})
