// 协议客户端和海投渲染的测试。
//
// 这里对面是个**假的桥**（就是本仓库 Rust 那侧的协议），不是假平台——平台那一段
// 用户有真的 OneBot，不在这里装。这个假桥只验一件事：我们说的帧，桥认不认。

import net from "node:net"
import { test } from "node:test"
import assert from "node:assert/strict"

import {
  BridgeError,
  BridgeLink,
  PROTOCOL_VERSION,
  normalizeToolArgs,
  parseEndpoint,
  personaSignature,
  renderElements,
  renderIssueUnread,
  renderPersona,
  renderPrompts,
  renderTurn,
} from "../plugin/acb-wire.mjs"
import {
  FakeBridge,
  TICKET,
  delivery,
  waitFor,
} from "./fake-bridge.mjs"

// ---------------------------------------------------------------------------
// 握手与轮次
// ---------------------------------------------------------------------------

/** 起一个假桥 + 一个客户端，测完保证都收掉（不然 node 不退出）。 */
async function setup(t, { onTurn, log } = {}) {
  const bridge = await new FakeBridge().start()
  const link = new BridgeLink({
    endpoint: bridge.endpoint,
    token: "t-main",
    harness: "opencode",
    onTurn,
    log,
  })
  t.after(async () => {
    link.close()
    await bridge.stop()
  })
  await link.connect()
  return { bridge, link }
}

/** 开一轮，并等到客户端真的拿到票。 */
async function openTurn(bridge, link, payload = delivery()) {
  await bridge.send({ frame: "turn_start", delivery: payload })
  await waitFor(() => link.ticket === payload.ticket, "客户端应该拿到本轮的票，拿到的是 " + link.ticket)
  return payload
}

test("握手带上协议版本和令牌，拿到会话组", async (t) => {
  const { bridge, link } = await setup(t)

  assert.deepEqual(link.sessions, ["main"])
  assert.deepEqual(bridge.hello, {
    frame: "hello",
    version: PROTOCOL_VERSION,
    harness: "opencode",
    token: "t-main",
  })
})

test("桥断了会自己重连，重连成功后重新绑上设备", async (t) => {
  const bridge = await new FakeBridge().start()
  const logs = []
  const link = new BridgeLink({
    endpoint: bridge.endpoint,
    token: "t-main",
    harness: "opencode",
    log: (level, message) => logs.push(`${level} ${message}`),
  })
  t.after(async () => {
    link.close()
    await bridge.stop()
  })
  await link.connect()

  // 桥重启（改配置就得重启，实测里真的会遇到）
  bridge.socket.destroy()

  await waitFor(
    () => logs.some((line) => line.includes("reconnected")),
    "应该自己重连回来，日志是：" + logs.join(" | "),
  )
  assert.deepEqual(link.sessions, ["main"], "重连之后会话要重新绑上，不然工具会一直 unauthorized")
})

test("开轮之后工具调用带着本轮的票，结果能对上号", async (t) => {
  const turns = []
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => turns.push(turn),
  })

  await openTurn(bridge, link)
  await waitFor(() => turns.length === 1, "onTurn 应该被叫到")

  const pending = link.call("send_message", {
    elements: [{ type: "text", text: "在的" }],
  })
  const call = await bridge.next()
  assert.equal(call.frame, "tool_call")
  assert.equal(call.tool, "send_message")
  assert.equal(call.ticket, TICKET)
  assert.deepEqual(call.args, { elements: [{ type: "text", text: "在的" }] })

  await bridge.send({
    frame: "tool_result",
    id: call.id,
    result: { platform_id: "p-1", watermark: 12 },
  })
  assert.deepEqual(await pending, { platform_id: "p-1", watermark: 12 })
})

test("工具失败带着错误码抛出来，不是悄悄返回空", async (t) => {
  const { bridge, link } = await setup(t, { onTurn: async () => {} })
  await openTurn(bridge, link)

  const pending = link.call("send_message_to", {
    elements: [{ type: "text", text: "喂" }],
    to: "qq:group/999",
  })
  const call = await bridge.next()
  await bridge.send({
    frame: "tool_result",
    id: call.id,
    error: { code: "room_not_subscribed", message: "没有订阅这个房间" },
  })

  await assert.rejects(pending, (error) => {
    assert.ok(error instanceof BridgeError, `应该是 BridgeError，实际是 ${error}`)
    assert.equal(error.code, "room_not_subscribed")
    assert.match(error.message, /没有订阅/)
    return true
  })
})

test("没有进行中的轮次时，工具调不动", async (t) => {
  const { link } = await setup(t)

  await assert.rejects(link.call("send_message", { text: "喂" }), (error) => {
    assert.equal(error.code, "unauthorized")
    return true
  })
})

test("同一连接上多个会话的轮次串行，不会串票", async (t) => {
  const seen = []
  let releaseFirst
  const blocked = new Promise((resolve) => {
    releaseFirst = resolve
  })
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => {
      seen.push(`start:${turn.session}`)
      if (turn.session === "a") await blocked
      const pending = link.call("send_message", { text: `回复 ${turn.session}` })
      let call = await bridge.next()
      // 上一轮的 turn_end 可能还排在队列里，只挑 tool_call。
      while (call.frame !== "tool_call") call = await bridge.next()
      seen.push(`call:${call.ticket}`)
      await bridge.send({ frame: "tool_result", id: call.id, result: {} })
      await pending
      link.endTurn(turn.ticket)
      seen.push(`end:${turn.session}`)
    },
  })

  await bridge.send({
    frame: "turn_start",
    delivery: delivery({
      ticket: "a-1",
      session: "a",
      room: { connection: "qq", path: ["group", "1"] },
    }),
  })
  await bridge.send({
    frame: "turn_start",
    delivery: delivery({
      ticket: "b-1",
      session: "b",
      room: { connection: "qq", path: ["group", "2"] },
    }),
  })

  await waitFor(() => seen.includes("start:a"), "第一轮应该先跑")
  assert.equal(link.ticket, "a-1", "后到的轮次不能覆盖正在跑的那张票")
  releaseFirst()
  await waitFor(() => seen.includes("end:b"), "两轮都应该跑完")
  assert.deepEqual(seen, [
    "start:a",
    "call:a-1",
    "end:a",
    "start:b",
    "call:b-1",
    "end:b",
  ])
})

test("收工发 turn_end，票随之作废", async (t) => {
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => {
      link.endTurn(turn.ticket)
    },
  })
  await bridge.send({ frame: "turn_start", delivery: delivery() })

  const end = await bridge.next()
  assert.deepEqual(end, { frame: "turn_end", ticket: TICKET })
  assert.equal(link.ticket, null)
})

test("收工可以带 v7 结构化失败分类", async (t) => {
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => {
      link.endTurn(turn.ticket, "gateway down", {
        component: "model",
        id: "openai/gpt-4",
        class: "model_unavailable",
      })
    },
  })
  await bridge.send({ frame: "turn_start", delivery: delivery() })

  const end = await bridge.next()
  assert.equal(end.frame, "turn_end")
  assert.equal(end.error, "gateway down")
  assert.deepEqual(end.failure, {
    component: "model",
    id: "openai/gpt-4",
    class: "model_unavailable",
  })
})

test("桥发 ping 要回 pong", async (t) => {
  const { bridge } = await setup(t)

  await bridge.send({ frame: "ping" })
  assert.deepEqual(await bridge.next(), { frame: "pong" })
})

// ---------------------------------------------------------------------------
// 渲染与参数
// ---------------------------------------------------------------------------

test("一轮摊成给模型看的文本", () => {
  const text = renderTurn(
    delivery({
      records: [
        delivery().records[0],
        {
          id: 13,
          room: { connection: "qq", path: ["group", "158296748"] },
          kind: { kind: "event", event: "poke", tags: ["poke", "notice"], summary: "[拍了拍我]" },
          sender: { id: "10002", name: "小明" },
          at: Date.now(),
          elements: [{ type: "text", text: "[拍了拍我]" }],
          at_self: true,
          redelivery: 2,
        },
      ],
    }),
    { now: Date.now() },
  )

  assert.match(text, /qq:group\/158296748/)
  assert.match(text, /#12/)
  assert.match(text, /老王\(10001\)/)
  assert.match(text, /冲你来的/)
  assert.match(text, /在吗/)
  assert.match(text, /事件 poke（标签 poke、notice）/)
  assert.match(text, /第 2 次投递/)
  assert.match(text, /ack_messages/)
})

test("wake_reason 写进开轮原因", () => {
  const text = renderTurn({
    ...delivery(),
    records: [],
    wake_reason: "@ 了我",
  })
  assert.match(text, /有新消息（@ 了我）/)
})

test("空记录的一轮也要说清楚是例行检查", () => {
  const text = renderTurn({ ...delivery(), records: [], reason: "clock" })
  assert.match(text, /定时例行检查/)
  assert.match(text, /没有新记录/)
})

test("issue 未读渲染成一行系统事件，只给前 3 条摘要", () => {
  const unread = {
    count: 4,
    event_ids: ["ev_1", "ev_2", "ev_3", "ev_4"],
    events: [
      { event_id: "ev_1", issue_id: "is_1", title: "登录失败", kind: "issue.created" },
      { event_id: "ev_2", issue_id: "is_2", title: "导出超时", kind: "issue.status_changed" },
      { event_id: "ev_3", issue_id: "is_3", title: "第三条", kind: "issue.comment_added" },
      { event_id: "ev_4", issue_id: "is_4", title: "第四条", kind: "issue.created" },
    ],
  }
  const line = renderIssueUnread(unread)
  assert.match(line, /\[系统事件 issue_unread\] 4 条未读/)
  assert.match(line, /is_1 登录失败/)
  assert.match(line, /is_3 第三条/)
  assert.doesNotMatch(line, /is_4 第四条/, "前 3 条之外只报计数")
  assert.match(line, /另有 1 条/)
  assert.match(line, /issue_my_work/)
  assert.equal(line.includes("\n"), false, "必须是一行")

  // 空记录轮也要带上这一行。
  const text = renderTurn({ ...delivery(), records: [], issue_unread: unread })
  assert.match(text, /\[系统事件 issue_unread\]/)
  assert.match(text, /没有新记录/)

  assert.equal(renderIssueUnread({ count: 0, events: [] }), "")
  assert.equal(renderIssueUnread(null), "")
})

test("插件上下文独立成块，排在记录之前并带来源", () => {
  const text = renderTurn({
    ...delivery(),
    prompts: [
      { source: "demo.lore", text: "这是设定", priority: 5 },
      { source: "demo.rules", text: "这是规则", priority: 1, truncated: true },
    ],
  })
  const block = text.indexOf("[插件上下文]")
  const record = text.indexOf("在吗")
  assert.ok(block >= 0, `应当有独立块：${text}`)
  assert.ok(block < record, `上下文要在记录之前：${text}`)
  assert.match(text, /\[来源: demo\.lore\]/)
  assert.match(text, /\[来源: demo\.rules\]/)
  // 片段不能拼进记录行，也不能冒充用户消息。
  assert.ok(!text.includes("[来源: demo.lore] 这是设定"), `片段要单独成行：${text}`)
})

test("旧桥没有 prompts 字段时输出完全不变", () => {
  const legacy = renderTurn(delivery())
  const empty = renderTurn({ ...delivery(), prompts: [] })
  assert.equal(empty, legacy)
  assert.ok(!legacy.includes("[插件上下文]"))
  assert.equal(renderPrompts(undefined), "")
  assert.equal(renderPrompts(null), "")
  assert.equal(renderPrompts([]), "")
})

test("人格卡渲染成一段可注入的 context 文本", () => {
  const text = renderPersona({
    id: "p",
    name: "默认人格",
    text: "你是桥。",
  })
  assert.match(text, /人格：默认人格/)
  assert.match(text, /你是桥。/)
  assert.match(text, /按它说话/)

  assert.equal(renderPersona(null), "")
  assert.equal(renderPersona({ id: "p", name: "", text: "   " }), "")

  // 签名按 id + 正文算：改一个字也要重新注入。
  assert.equal(personaSignature(null), "")
  assert.notEqual(
    personaSignature({ id: "p", text: "甲" }),
    personaSignature({ id: "p", text: "乙" }),
  )
})

test("引用我的话单独标出来，和 @ 分开说", () => {
  const record = {
    ...delivery().records[0],
    id: 7,
    at_self: false,
    reply_to_self: true,
    elements: [{ type: "quote", message_id: "592852179" }, { type: "text", text: "只发子网" }],
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })

  assert.match(text, /回你的话/, "引用要是看不出来，模型就不知道人在追问哪件事")
  assert.ok(!text.includes("冲你来的"), `引用不是 @，别混着说：${text}`)
})

test("桥算好了引用对齐就并排显示，模型不用自己映射平台编号", () => {
  const record = {
    ...delivery().records[0],
    id: 621,
    at_self: true,
    elements: [{ type: "quote", message_id: "-1179324851" }, { type: "text", text: "对的对的" }],
    quote_ref: { platform_id: "-1179324851", id: 117 },
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })
  assert.match(text, /\[引用 #117\]/, `该用本房间编号：${text}`)
  assert.ok(!text.includes("-1179324851"), `平台编号不该再出现在正文里：${text}`)
})

test("引用的是本房间没有的消息时要说实话，不能瞎给编号", () => {
  const record = {
    ...delivery().records[0],
    id: 622,
    elements: [{ type: "quote", message_id: "gone" }, { type: "text", text: "？" }],
    quote_ref: { platform_id: "gone" },
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })
  assert.match(text, /\[引用 #gone，本房间历史里没有这条\]/)
})

test("被撤回的消息在轮次里明确标记", () => {
  const record = {
    ...delivery().records[0],
    id: 8,
    labels: ["recall"],
    elements: [{ type: "text", text: "这条后来被撤回了" }],
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })

  assert.match(text, /已撤回/, "撤回标记丢了，模型会把撤回的话当成还能看到的内容")
})

test("桥的旁白单独一行，不拼进对方说的话里", () => {
  const record = {
    ...delivery().records[0],
    id: 5,
    elements: [{ type: "text", text: "1" }],
    redelivery: 2,
    note: "第 2 次投递；调 ack_messages 确认 #5 之后就不再重复投递",
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })
  const lines = text.split("\n")

  assert.ok(
    lines.some((line) => line.trim() === "1"),
    `内容那行必须只有「1」，实际是：${JSON.stringify(lines)}`,
  )
  assert.ok(
    lines.some(
      (line) =>
        line.trim().startsWith("※") &&
        line.includes("第 2 次投递") &&
        line.includes("#5") &&
        line.includes("ack_messages"),
    ),
    `旁白要单独一行且说清怎么止投：${JSON.stringify(lines)}`,
  )
  // 有 note 就不在标题里重复一遍「第 N 次投递」。
  const header = lines.find((line) => line.startsWith("#5"))
  assert.ok(!header.includes("第 2 次投递"), `标题别重复：${header}`)
})

test("内容元素都摊得开，平台内容原样摊给模型", () => {
  const text = renderElements([
    { type: "text", text: "看图 " },
    { type: "image", url: "https://x/1.png" },
    { type: "at", target: "10001" },
    { type: "at", all: true },
    { type: "quote", message_id: "9" },
    { type: "face", platform: "qq", id: "14" },
    {
      type: "opaque",
      platform: "qq",
      payload: { type: "forward", data: { id: "abc123" } },
    },
  ])
  assert.match(text, /看图/)
  assert.match(text, /\[图片 https:\/\/x\/1\.png\]/)
  assert.match(text, /@10001/)
  assert.match(text, /@全体成员/)
  assert.match(text, /\[引用 #9\]/)
  assert.match(text, /\[表情 14\]/)
  // 桥不解释平台语义，但原始载荷必须送到模型眼前，不能只剩一句占位。
  assert.match(text, /qq 平台特有内容/)
  assert.match(text, /"type":"forward"/)
  assert.match(text, /"id":"abc123"/)
  // 还要带上「可以怎么取」：只给一个 id，模型不知道自己能查（ADR-0020）。
  assert.match(text, /可调 platform_call/)
  assert.match(text, /msg\.forward/)
})

test("不认识的平台内容也会指向能力目录，不让模型瞎猜 op", () => {
  const text = renderElements([
    { type: "opaque", platform: "kook", payload: { type: "card", data: {} } },
  ])
  assert.match(text, /platform_call/)
  // 没声明对应 op 的类型，不该瞎编一个 get_content 出来。
  assert.doesNotMatch(text, /get_content/)
})

test("平台载荷过长会截断，但绝不整段吞掉", () => {
  const huge = "x".repeat(2000)
  const text = renderElements([
    { type: "opaque", platform: "kook", payload: { card: huge } },
  ])
  assert.match(text, /kook 平台特有内容/)
  assert.match(text, /已截断/)
  assert.ok(text.length < 1000, `不能把上下文撑爆：${text.length}`)
})

test("事件记录里的原始载荷不摊开，只给流水编号让 agent 自己查", () => {
  // 未解析事件的降级存法（ADR-0004 补充裁定 2026-10-31）：载荷存流水，
  // 投递不塞上下文。渲染要给编号 + 查法，否则模型不知道细节去哪找；
  // 但只有本轮工具面里真的有 fetch_records 时才推荐它，避免 unknown tool。
  const record = {
    id: 87,
    room: { connection: "qq", path: ["group", "158296748"] },
    kind: { kind: "event", event: "group_msg_emoji_like", tags: ["notice", "reaction"] },
    sender: { id: "10002", name: "小明" },
    at: Date.now(),
    elements: [
      { type: "text", text: "[group_msg_emoji_like]（未解析事件）" },
      { type: "opaque", platform: "qq", payload: { notice_type: "group_msg_emoji_like", emoji_id: "5" } },
    ],
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })

  assert.match(text, /#87/)
  assert.ok(!text.includes("fetch_records"), `工具面没有 fetch_records 时不能推荐：${text}`)
  assert.ok(!text.includes("emoji_id"), `原始载荷不该进上下文：${text}`)

  const withTool = renderTurn(
    { ...delivery(), records: [record] },
    { now: Date.now(), tools: ["fetch_records"] },
  )
  assert.match(withTool, /fetch_records/, "工具存在时才给具体查法")
})

test("投递超限提示按连续投递描述，工具面没有回看工具时不推荐", () => {
  const truncated = { ...delivery(), more: 3 }
  const text = renderTurn(truncated, { now: Date.now() })
  assert.match(text, /从旧到新的一段连续记录/)
  assert.match(text, /剩余记录会按顺序在后续轮次补齐/)
  assert.ok(!text.includes("fetch_records"), `工具面没有 fetch_records 时不能推荐：${text}`)

  const withTool = renderTurn(truncated, { now: Date.now(), tools: ["fetch_records"] })
  assert.match(withTool, /fetch_records/)
})

test("消息记录里的 opaque 照旧摊开，不受事件降级影响", () => {
  const record = {
    ...delivery().records[0],
    id: 88,
    elements: [
      { type: "opaque", platform: "qq", payload: { type: "forward", data: { id: "abc" } } },
    ],
  }
  const text = renderTurn({ ...delivery(), records: [record] }, { now: Date.now() })
  assert.match(text, /qq 平台特有内容/, "消息里的平台内容必须摊开（ADR-0004）")
  assert.match(text, /"id":"abc"/)
})

test("endpoint 只认 host:port", () => {
  assert.deepEqual(parseEndpoint("127.0.0.1:7700"), {
    host: "127.0.0.1",
    port: 7700,
  })
  assert.throws(() => parseEndpoint("127.0.0.1"), /127\.0\.0\.1:7700/)
  assert.throws(() => parseEndpoint("127.0.0.1:abc"), /port/)
})

test("text 是单段文本的糖，elements 才是本体", () => {
  assert.deepEqual(normalizeToolArgs("send_message", { text: "在的" }), {
    elements: [{ type: "text", text: "在的" }],
  })
  assert.deepEqual(
    normalizeToolArgs("send_message", {
      elements: [{ type: "text", text: "a" }, { type: "at", target: "1" }],
    }),
    { elements: [{ type: "text", text: "a" }, { type: "at", target: "1" }] },
  )
  assert.throws(
    () => normalizeToolArgs("send_message", { elements: [{ type: "sticker" }] }),
    /unknown element type/,
  )
  assert.throws(() => normalizeToolArgs("send_message", {}), /requires text/)
})

test("发到别的房间必须有 to", () => {
  assert.deepEqual(
    normalizeToolArgs("send_message_to", { to: "qq:group/1", text: "喂" }),
    { elements: [{ type: "text", text: "喂" }], to: "qq:group/1" },
  )
  assert.throws(() => normalizeToolArgs("send_message_to", { text: "喂" }), /requires to/)
})

test("send_message_to 不再有 device 身份参数（跨会话发送已砍）", () => {
  // 多传的 device 会被忽略——桥这边根本不会再把它透过去。
  assert.deepEqual(
    normalizeToolArgs("send_message_to", {
      to: "qq:group/1",
      device: "helper",
      text: "喂",
    }),
    { elements: [{ type: "text", text: "喂" }], to: "qq:group/1" },
  )
})

test("确认的编号必须是整数，空的不算", () => {
  assert.deepEqual(normalizeToolArgs("ack_messages", { ids: [1, 2] }), {
    ids: [1, 2],
  })
  assert.throws(() => normalizeToolArgs("ack_messages", { ids: [] }), /requires ids/)
  assert.throws(
    () => normalizeToolArgs("ack_messages", { ids: ["1"] }),
    /must be integers/,
  )
})

test("其它工具原样透传，顺手去掉空字段", () => {
  assert.deepEqual(
    normalizeToolArgs("get_history", { limit: 5, room: undefined }),
    { limit: 5 },
  )
  assert.deepEqual(
    normalizeToolArgs("notes", {
      op: "set",
      key: "k",
      value: "v",
      room: "qq:group/1",
      status: null,
    }),
    { op: "set", key: "k", value: "v", room: "qq:group/1" },
  )
})

