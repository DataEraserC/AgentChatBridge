// 测试共用的「假桥」与等待帮手。
//
// 对面是**假的桥**（本仓库 Rust 那侧的协议），不是假平台——平台那一段用户有真的
// OneBot，不在这里装。假桥只验一件事：我们说的帧，桥认不认。

import net from "node:net"
import assert from "node:assert/strict"

import { RESIDENT_TOOL_NAMES, TOOLS } from "../../shared/tools.mjs"

export const TICKET = "main-1"

export function delivery(overrides = {}) {
  return {
    ticket: TICKET,
    session: "main",
    room: { connection: "qq", path: ["group", "158296748"] },
    reason: "wake",
    records: [
      {
        id: 12,
        room: { connection: "qq", path: ["group", "158296748"] },
        kind: { kind: "message" },
        sender: { id: "10001", name: "老王" },
        at: Date.now(),
        elements: [{ type: "text", text: "在吗" }],
        at_self: true,
      },
    ],
    session_info: { id: "main", name: "main", harness: "opencode", model: null },
    ...overrides,
  }
}

/**
 * 测试用 surface：常驻白名单 + 长尾目录。`extensions_*` 默认不可直接执行，
 * 用来验证隐藏工具仍留在目录里但不进可见集。
 *
 * 形状与 core 的瘦身视图一致：可执行条目只有 `tool` + `purpose`，
 * 不可执行条目才带 `reason` / `requestable`；不再重复列 `visible` / `hidden`。
 */
export function surface(overrides = {}) {
  const resident = overrides.resident ?? RESIDENT_TOOL_NAMES
  const catalog =
    overrides.catalog ??
    TOOLS.filter((tool) => !resident.includes(tool.name)).map((tool) => {
      if (tool.name.startsWith("extensions_")) {
        return {
          tool: tool.name,
          direct_executable: false,
          requestable: false,
          ticket_required: false,
          reason: "admin_only",
        }
      }
      return {
        tool: tool.name,
        purpose: `测试用途：${tool.name}`,
      }
    })
  const visibleCount = catalog.filter(
    (entry) => entry.direct_executable !== false,
  ).length
  const hiddenCount = catalog.length - visibleCount
  return {
    scope: "surface",
    session: "main",
    room: "qq:group/158296748",
    subject: "user",
    place: "group",
    resident: resident,
    catalog,
    counts: {
      tools_total: TOOLS.length,
      resident: resident.length,
      catalog: catalog.length,
      visible: resident.length + visibleCount,
      hidden: hiddenCount,
    },
    ...overrides,
  }
}

export class FakeBridge {
  constructor({
    sessions = ["main"],
    token = "t-main",
    surface: surfaceFixture = surface(),
    failSurface = false,
    issueUnread = { count: 0, event_ids: [], events: [] },
    failIssue = false,
  } = {}) {
    this.sessions = sessions
    this.token = token
    this.surface = surfaceFixture
    this.failSurface = failSurface
    this.issueUnread = issueUnread
    this.failIssue = failIssue
    /** harness 的 issue_my_work 内部调用（读取 + ack 都在这里）。 */
    this.issueCalls = []
    this.frames = []
    this.socket = null
    this.connected = null
    this.#connected = new Promise((resolve) => {
      this.connected = resolve
    })
  }

  #connected

  async start() {
    this.sockets = new Set()
    this.server = net.createServer((socket) => {
      this.socket = socket
      this.sockets.add(socket)
      socket.setEncoding("utf8")
      this.lines = new LineQueue(socket, (frame) => {
        // 工具面查询是 harness 内部动作：回了就到，不进 next() 的轮次帧序列。
        if (
          frame.frame === "tool_call" &&
          frame.tool === "capabilities" &&
          frame.args?.scope === "surface"
        ) {
          this.frames.push(frame)
          if (this.failSurface) {
            this.#write({
              frame: "tool_result",
              id: frame.id,
              error: { code: "internal", message: "surface unavailable" },
            })
          } else {
            this.#write({ frame: "tool_result", id: frame.id, result: this.surface })
          }
          return true
        }
        // capabilities scope=tool：给一个最小权限结果，让 harness 拼本地 schema。
        if (
          frame.frame === "tool_call" &&
          frame.tool === "capabilities" &&
          frame.args?.scope === "tool" &&
          frame.args?.name
        ) {
          this.frames.push(frame)
          this.#write({
            frame: "tool_result",
            id: frame.id,
            result: {
              scope: "tool",
              kind: "tool",
              tool: frame.args.name,
              layer: "catalog",
              available: true,
              direct_executable: true,
              requestable: frame.args.name === "notes",
              ticket_required: false,
              reason: null,
            },
          })
          return true
        }
        // 首轮 issue 未读补查：harness 走 plugin_tool_call → issue_my_work。
        // 读取和 ack 都是内部动作，不进 next() 的轮次帧序列。
        if (
          frame.frame === "tool_call" &&
          frame.tool === "plugin_tool_call" &&
          frame.args?.tool === "issue_my_work"
        ) {
          this.frames.push(frame)
          this.issueCalls.push(frame)
          if (this.failIssue) {
            this.#write({
              frame: "tool_result",
              id: frame.id,
              error: { code: "internal", message: "issue service unavailable" },
            })
          } else if (Array.isArray(frame.args?.arguments?.ack_event_ids)) {
            this.#write({
              frame: "tool_result",
              id: frame.id,
              result: {
                ok: true,
                acked: frame.args.arguments.ack_event_ids.length,
              },
            })
          } else {
            this.#write({
              frame: "tool_result",
              id: frame.id,
              result: { work_items: [], unread_events: this.issueUnread },
            })
          }
          return true
        }
        return false
      })
      this.connected()
      // 收尾时新连接可能还挂着等 hello，别让它变成未处理的 rejection。
      void this.#greet().catch(() => {})
    })
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve))
    this.port = this.server.address().port
    return this
  }

  /** 等客户端真的连上来（插件是异步连接的，测试得等它）。 */
  whenConnected() {
    return this.#connected
  }

  get endpoint() {
    return `127.0.0.1:${this.port}`
  }

  async #greet() {
    const hello = await this.lines.next()
    this.hello = hello
    this.#write({ frame: "ready", sessions: this.sessions })
  }

  async next() {
    // report_map 的 control_result 是插件建会话后的旁路回报，不属于轮次帧。
    // 测试关心的是 turn_end / tool_call，跳过它，别让每个用例都先撞一帧。
    for (;;) {
      const frame = await this.lines.next()
      this.frames.push(frame)
      if (frame.frame === "control_result") continue
      return frame
    }
  }

  async send(frame) {
    this.#write(frame)
  }

  #write(frame) {
    this.socket.write(`${JSON.stringify(frame)}\n`)
  }

  async stop() {
    for (const socket of this.sockets ?? []) socket.destroy()
    this.sockets?.clear()
    await new Promise((resolve) => this.server.close(resolve))
  }
}

class LineQueue {
  constructor(socket, onMessage = () => false) {
    this.buffer = ""
    this.waiting = []
    this.lines = []
    this.onMessage = onMessage
    socket.on("data", (chunk) => {
      this.buffer += chunk
      for (;;) {
        const at = this.buffer.indexOf("\n")
        if (at < 0) return
        const line = this.buffer.slice(0, at)
        this.buffer = this.buffer.slice(at + 1)
        const value = JSON.parse(line)
        if (!this.onMessage(value)) this.#push(value)
      }
    })
  }

  #push(value) {
    const waiter = this.waiting.shift()
    if (waiter) waiter(value)
    else this.lines.push(value)
  }

  next() {
    if (this.lines.length) return Promise.resolve(this.lines.shift())
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("假桥等帧超时")), 2000)
      this.waiting.push((value) => {
        clearTimeout(timer)
        resolve(value)
      })
    })
  }
}

export async function waitFor(predicate, message) {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}
