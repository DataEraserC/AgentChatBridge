// acb 的 opencode v2 插件：把「桥推来的一轮」变成「opencode 会话里的一次提问」，
// 同时把桥的九个工具挂进这个实例，让模型直接看见它们。
//
// 装法：把这个目录整体放到 `.opencode/plugins/acb/`（或 `~/.config/opencode/plugins/acb/`）。
// v2 认目录里的 `index.js`，两个文件必须挨着——`index.js` 会 import `acb-wire.mjs`。
//
// 配置走插件选项（opencode.jsonc）或环境变量，选项优先：
//   endpoint  桥的接入点，默认 127.0.0.1:7700
//   token     设备令牌，必填；和 config.toml 里设备的 token 一致
//   session   钉死用哪个会话；不填就按设备找上次那个
//
// 为什么是这种写法：opencode v2 的插件是 `export default { id, setup(ctx) }`，
// 工具用 `ctx.tool.transform` 注册、参数是 JSON Schema。v1 的
// `export const X = async ({client}) => ({tool})` 在 v2 里**不会被加载**（实测），
// 所以这里不兼容 v1——见 docs/06 与 ADR-0013。

import {
  BridgeError,
  BridgeLink,
  normalizeToolArgs,
  personaSignature,
  renderPersona,
  renderTurn,
  roomText,
} from "./acb-wire.mjs"
import {
  RESIDENT_TOOL_NAMES,
  toolsForSurface,
  withLocalSchema,
} from "../../shared/tools.mjs"
import { t } from "../../shared/i18n.mjs"
import fs from "node:fs"
import path from "node:path"

const SERVICE = "acb"

/**
 * `provider/model` → opencode 的 {providerID, modelID}。按第一个 `/` 拆：
 * provider 名不含 `/`，模型 id 允许含 `/`（如 `1Agent2Api/glm-5.3-flash`）。
 */
function parseModelRef(text) {
  const value = String(text ?? "").trim()
  if (!value) return null
  const slash = value.indexOf("/")
  if (slash <= 0 || slash === value.length - 1) {
    throw new Error(t("opencode.model_ref_invalid", { value }))
  }
  return { providerID: value.slice(0, slash), modelID: value.slice(slash + 1) }
}

/**
 * 把 opencode/模型侧错误翻成 v7 的结构化失败分类。只有明确线索才分类；
 * 业务错误（内容拦截、参数 400）返回 null，不污染依赖表。
 */
function classifyTurnError(error) {
  const code = String(error?.code ?? "")
    .toLowerCase()
    .replaceAll("_", "-")
  const message = String(error?.message ?? error ?? "").toLowerCase()
  if (
    code.includes("model-unavailable") ||
    /econnrefused|model[^.]*(unavailable|refused)|gateway[^.]*(unavailable|refused)/.test(
      message,
    )
  ) {
    return { component: "model", class: "model_unavailable" }
  }
  if (
    code.includes("model-unconfigured") ||
    /model[^.]*(not found|unknown|not configured)|provider[^.]*(not found|not configured)/.test(
      message,
    )
  ) {
    return { component: "model", class: "model_unconfigured" }
  }
  if (code.includes("auth") || /401|403|unauthorized|forbidden|invalid api key|credentials/.test(message)) {
    return { component: "model", class: "auth_failed" }
  }
  if (code.includes("rate") || /429|rate limit|quota/.test(message)) {
    return { component: "model", class: "rate_limited" }
  }
  return null
}

/**
 * opencode 报的错是不是「这个会话记录的工作目录已经不存在」。
 *
 * `session.get` 只返回记录，不校验目录；真正投话时 opencode 才报
 * `Directory not found: /path`。这类会话不能继续用，必须换一条新的；
 * 但模型 400、桥错误不能触发重建，否则每轮失败都会丢上下文。
 */
export function isMissingDirectoryError(error) {
  const message = String(error?.message ?? error)
  return /directory[^,;]*?(?:not[ -]?found|does[ -]?not[ -]?exist|no[ -]?such|missing)|not[ -]?found[^,;]*?directory/i.test(
    message,
  )
}

export default {
  id: SERVICE,

  async setup(ctx) {
    const config = readConfig(ctx)
    const log = makeLogger(ctx, config.logFile)
    log(
      "info",
      t("opencode.plugin_load", {
        endpoint: config.endpoint,
        harness: config.harness,
        session: config.session || t("opencode.session_from_map"),
        logFile: config.logFile,
      }),
    )

    if (!config.token) {
      log("warn", t("opencode.no_token_disabled"))
      return
    }

    const driver = new TurnDriver({ ctx, log, pinned: config.session })
    const link = new BridgeLink({
      endpoint: config.endpoint,
      token: config.token,
      harness: config.harness,
      log,
      onTurn: (delivery) => driver.run(delivery, link),
      onControl: (frame) => driver.handleControl(frame, link),
    })

    // 工具面先挂常驻白名单：长尾工具开轮后按桥的 surface 再加，避免启动时
    // 把 20 多个长尾 schema 一次塞进 Code Mode 目录。
    const makeSpec = (spec) => ({
      ...spec,
      execute: async (input) => ({
        content: await callBridge(link, spec.name, input ?? {}),
      }),
    })
    const residentSpecs = toolSpecsForSurface({
      resident: RESIDENT_TOOL_NAMES,
      visible: RESIDENT_TOOL_NAMES,
      catalog: [],
    })
    const registration = await ctx.tool.transform((editor) => {
      for (const spec of residentSpecs) {
        editor.add(makeSpec(spec))
      }
    })
    driver.visibleNames = new Set(residentSpecs.map((spec) => spec.name))
    driver.applySurface = async (surface) => {
      const desired = toolSpecsForSurface(surface)
      const desiredNames = new Set(desired.map((spec) => spec.name))
      await ctx.tool.transform((editor) => {
        for (const name of driver.visibleNames) {
          if (!desiredNames.has(name)) editor.remove(name)
        }
        for (const spec of desired) {
          if (!driver.visibleNames.has(spec.name)) editor.add(makeSpec(spec))
        }
      })
      driver.visibleNames = desiredNames
      driver.surface = surface
    }

    void link
      .connect()
      .then(async () => {
        log("info", t("opencode.connected", { sessions: link.sessions.join(", ") }))
        // 把 storage 里还活着的映射主动报给桥：控制台的「harness 侧映射」
        // 靠 report_map 落盘，不报的话页面上永远是空、也没法断开。
        await driver.reportStoredMaps(link)
      })
      .catch((error) => log("warn", t("opencode.bridge_unreachable", { error: error.message })))

    return async () => {
      link.close()
      log("warn", t("opencode.plugin_unloaded"))
      try {
        await registration?.dispose?.()
      } catch {
        // 卸载时 dispose 失败不值得让进程起不来。
      }
    }
  },
}

// ---------------------------------------------------------------------------
// 一轮：投递 → 提问 → 等它说完 → 收工
// ---------------------------------------------------------------------------

/**
 * opencode v2 里插件注册的工具**不在**模型的原生工具列表里，而是进「Code Mode」目录，
 * 模型得用 `execute` 工具跑一行 JS 才调得动：`await tools.send_message({ text: "..." })`。
 *
 * 实测（2026-10-29，nemotron-3-ultra）：模型看到投递里「回这个房间用 send_message」，
 * 第一反应是直接调 `send_message`，得到
 * `No tool named "send_message" is currently available`，第二次才想起来走 `execute`。
 * 每轮白失败一次很浪费，所以在投递末尾把这条规矩直接说清楚。
 * 这是 opencode 独有的，属于适配器的事，不往通用的 renderTurn 里塞。
 */
const CODE_MODE_HINT = [
  "",
  "---",
  "（opencode 提示：上列工具不在原生工具列表里，要用 execute 跑一行 JS：",
  '  发消息 `await tools.send_message({ text: "收到" })`；',
  "  不带 `to` 就是发到本轮房间；不要因为上下文里聊过别的群，就自己补一个 `to`。",
  "  确认 `await tools.ack_messages({ ids: [1, 2] })`；",
  "  长尾工具（记忆、笔记、任务、反馈、扩展、插件）不常驻：先",
  '  `await tools.capabilities({ scope: "catalog" })` 看索引；',
  '  要看完整参数用 `await tools.capabilities({ scope: "tool", name: "..." })`。',
  "  先想清楚要发什么，再一次调用，别先用原生工具名硬调。",
  "",
  "  平台特有的能力（读合并转发正文、查群成员、发图、审批入群）从目录查，别自己猜：",
  '  `await tools.platform_call({ query: "图片" })` 看有哪些 op、参数怎么填，',
  '  再 `await tools.platform_call({ op: "msg.send", params: {...} })`。',
  "  别用 fetch 直接连桥或聊天平台的端口：那样绕过了订阅校验和水位，发出的消息",
  "  不进归档，下一轮你对这段对话就失忆了。）",
].join("\n")

class TurnDriver {
  constructor({ ctx, log, pinned }) {
    this.ctx = ctx
    this.log = log
    this.pinned = pinned || ""
    this.sessions = new Map()
    /** 已向桥 report_map 过的会话 → opencode 会话 id，防重复回报。 */
    this.reported = new Map()
    /** 会话 id → 已注入过的人格签名；人格只在首轮/变更时注入一次。 */
    this.personas = new Map()
    /** 串行链：同一会话本来就串行，这里再兜一层，免得两轮叠在一起。 */
    this.chain = Promise.resolve()
    /** 本轮可见工具面；启动时只有常驻白名单，开轮后按桥的 surface 更新。 */
    this.surface = {
      resident: RESIDENT_TOOL_NAMES,
      visible: RESIDENT_TOOL_NAMES,
      catalog: [],
    }
    /** setup 里注入：用新的 surface 调 ctx.tool.transform 增删工具。 */
    this.applySurface = null
    /** 当前已注册的工具名，避免重复 add / 漏 remove。 */
    this.visibleNames = new Set(RESIDENT_TOOL_NAMES)
  }

  /**
   * 开轮后、投话前向桥要一次 surface。失败只记日志并沿用上一次的工具面：
   * 工具面是辅助信息，不能因为它把一轮对话拖死。
   */
  async #refreshSurface(link, session) {
    try {
      const result = await callBridge(
        link,
        "capabilities",
        { scope: "surface" },
        { internal: true },
      )
      const surface = JSON.parse(result)
      if (
        !surface ||
        !Array.isArray(surface.resident) ||
        !Array.isArray(surface.visible) ||
        !Array.isArray(surface.catalog)
      ) {
        throw new Error("tool surface shape is invalid")
      }
      await this.applySurface?.(surface)
      this.surface = surface
      this.log(
        "info",
        t("opencode.tool_surface_loaded", {
          session,
          resident: surface.counts?.resident ?? surface.resident.length,
          visible: surface.counts?.visible ?? surface.visible.length,
          hidden: surface.counts?.hidden ?? surface.hidden?.length ?? 0,
        }),
      )
    } catch (error) {
      this.log(
        "warn",
        t("opencode.tool_surface_failed", { error: error?.message ?? String(error) }),
      )
    }
  }

  /**
   * 把 storage 里还活着的映射主动报给桥（report_map）。连接建立后调一次，
   * 让控制台立刻看得见 harness 侧映射——哪怕一个轮次都还没跑过。
   */
  async reportStoredMaps(link) {
    for (const session of link.sessions) {
      if (this.pinned) continue
      try {
        const stored = await this.#storageGet(`session:${session}`)
        if (!stored || !(await this.#alive(stored))) continue
        this.sessions.set(session, stored)
        this.#reportMap(session, stored, link)
      } catch (error) {
        this.log("warn", t("opencode.report_mapping_failed", { session, error: error?.message ?? error }))
      }
    }
  }

  /** 向桥回报映射（桥侧落盘，控制台可见）。同一个 (会话, id) 只报一次。 */
  #reportMap(session, id, link) {
    if (this.reported.get(session) === id) return
    this.reported.set(session, id)
    try {
      link.controlResult({
        // proto 里 control_result 的 id 是必需字段（缺了整帧被拒、连接被掐）；
        // 主动回报没有原帧可配对，学 dsh 用合成 id。
        id: `map-${Date.now()}`,
        session,
        command: "report_map",
        result: { session_id: id, meta: { title: `acb:${session}` } },
      })
      this.log("info", t("opencode.mapping_reported", { session, id }))
    } catch (error) {
      this.log("warn", t("opencode.mapping_report_failed", { error: error?.message ?? error }))
    }
  }

  /**
   * 桥推来的控制指令。
   *
   * - `interrupt`：打断当前正在跑的轮次（管理员 `/stop` 或控制台的「打断」按钮）。
   * - `reset`：忘掉旧映射（storage 一起清），立刻新建一条干净的 opencode 会话
   *   并把新 id 报回桥——桥删旧映射、存新映射，控制台马上能看到换好的映射。
   */
  async handleControl(frame, link) {
    const { id, session, command } = frame
    try {
      if (command === "interrupt") {
        const sessionID = this.pinned || this.sessions.get(session)
        if (!sessionID) {
          throw new Error(t("opencode.no_mapping", { session }))
        }
        if (typeof this.ctx.session.interrupt !== "function") {
          throw new Error(t("opencode.no_interrupt"))
        }
        await this.ctx.session.interrupt({ sessionID })
        link.controlResult({
          id,
          session,
          command,
          result: { ok: true, session_id: sessionID },
        })
        this.log("info", t("opencode.interrupted", { session, sessionID }))
        return
      }
      if (command !== "reset") throw new Error(t("opencode.unknown_command", { command }))
      this.#forget(session)
      let newId = ""
      if (this.pinned) {
        this.log("warn", t("opencode.pinned_reset_warning", { session, pinned: this.pinned }))
      } else {
        // storage 刚清掉，#session 在这里必然走新建分支。
        newId = await this.#session(session)
      }
      link.controlResult({
        id,
        session,
        command,
        result: { ok: true, ...(newId ? { session_id: newId } : {}) },
      })
      this.log(
        "info",
        newId
          ? t("opencode.session_reset_new", { session, sessionId: newId })
          : t("opencode.session_reset", { session }),
      )
    } catch (error) {
      const message = error?.message ?? String(error)
      this.log("error", t("opencode.reset_failed", { session, error: message }))
      link.controlResult({ id, session, command, error: message })
    }
  }

  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    let turnFailureInfo = null
    const started = Date.now()
    try {
      let sessionID = await this.#session(delivery.session)
      await this.#refreshSurface(link, delivery.session)
      // 映射（新建或接上的）报给桥，控制台才看得到 harness 侧映射。
      this.#reportMap(delivery.session, sessionID, link)
      let text = renderTurn(delivery) + CODE_MODE_HINT
      // 人格只在首轮或变更时注入：桥只给语义，opencode 这边把它放在本轮最前面。
      // 重复注入会白烧 token，所以按签名去重。
      const personaText = renderPersona(delivery.persona)
      const signature = personaSignature(delivery.persona)
      if (personaText && this.personas.get(delivery.session) !== signature) {
        const updated = this.personas.has(delivery.session) ? "[人格已更新]\n" : ""
        text = `${updated}${personaText}\n\n${text}`
        this.personas.set(delivery.session, signature)
        this.log("info", t("opencode.persona_injected", {
          persona: delivery.persona?.id ?? "",
          session: delivery.session,
        }))
      }
      const info = delivery.session_info ?? {}
      const modelRef = parseModelRef(info.model)
      const model = modelRef
        ? `${modelRef.providerID}/${modelRef.modelID}`
        : t("opencode.model_default")
      this.log(
        "info",
        t("opencode.turn_started", {
          ticket: delivery.ticket,
          room: roomText(delivery.room),
          count: delivery.records.length,
          sessionID,
          model,
        }),
      )
      // prompt 把话排进会话；wait 等这一轮助手真正说完。
      // 会话配了 model 就随本轮带上，opencode 会自己校验 provider/model 是否已配置。
      const sendPrompt = () =>
        this.ctx.session.prompt({
          sessionID,
          text,
          ...(modelRef ? { model: modelRef } : {}),
        })
      try {
        await sendPrompt()
      } catch (error) {
        if (!isMissingDirectoryError(error)) throw error
        // 旧会话记录指向的目录已被删掉：`session.get` 不校验目录，只有 prompt 才会报。
        // 丢掉旧映射，新建一条会话重试一次；仍失败就走正常的轮次失败上报。
        this.log(
          "warn",
          t("opencode.session_directory_missing", {
            session: delivery.session,
            sessionId: sessionID,
            error: error?.message ?? error,
          }),
        )
        this.#forget(delivery.session)
        sessionID = await this.#session(delivery.session)
        this.#reportMap(delivery.session, sessionID, link)
        await sendPrompt()
      }
      this.log("info", t("opencode.prompt_sent", { ticket: delivery.ticket }))
      if (typeof this.ctx.session.wait === "function") {
        await this.ctx.session.wait({ sessionID })
        await this.#assertTurnSucceeded(sessionID)
      }
      this.log(
        "info",
        t("opencode.turn_complete", {
          ticket: delivery.ticket,
          seconds: ((Date.now() - started) / 1000).toFixed(1),
          calls: link.turnCalls ?? 0,
        }),
      )
      if (delivery.records.length > 0 && (link.turnCalls ?? 0) === 0) {
        this.log(
          "warn",
          t("opencode.no_tool_calls", { ticket: delivery.ticket }),
        )
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      const failureInfo = classifyTurnError(error)
      this.log(
        "error",
        t("opencode.turn_failed_elapsed", {
          ticket: delivery.ticket,
          error: failure,
          seconds: ((Date.now() - started) / 1000).toFixed(1),
        }),
      )
      // 会话可能被删了：下次重建，别一直往一个死会话里灌。
      this.#forget(delivery.session)
      turnFailureInfo = failureInfo
    } finally {
      try {
        link.endTurn(delivery.ticket, failure, turnFailureInfo)
      } catch (error) {
        this.log("warn", t("opencode.turn_end_failed", { error: error.message }))
      }
    }
  }

  /**
   * 这个会话的 opencode 会话：钉死的 > 记住的 > 新建。
   *
   * 串行键是会话（ADR-0017），这里按会话 id 找/建 opencode 会话，保证同一会话
   * 的轮次都落在同一条 opencode 会话里。重开 opencode 要能接上上次那条——v2 没有
   * 「按标题列会话」，把会话 id 存进插件自己的 storage。
   */
  async #session(session) {
    if (this.pinned) return this.pinned

    // 缓存里的也要确认还活着：用户可能在 opencode 里把会话删了。
    const cached = this.sessions.get(session)
    if (cached && (await this.#alive(cached))) return cached
    if (cached) this.#forget(session)

    const stored = await this.#storageGet(`session:${session}`)
    if (stored && (await this.#alive(stored))) {
      this.sessions.set(session, stored)
      this.log("info", t("opencode.session_reused", { sessionId: stored, session }))
      return stored
    }

    const created = await this.ctx.session.create({ title: `acb:${session}` })
    const id =
      created?.id ??
      created?.sessionID ??
      created?.session?.id ??
      created?.data?.id
    if (!id) {
      throw new Error(t("opencode.session_no_id", { result: JSON.stringify(created).slice(0, 200) }))
    }
    this.sessions.set(session, id)
    await this.#storageSet(`session:${session}`, id)
    this.log("info", t("opencode.session_created", { sessionId: id, session }))
    return id
  }

  #forget(session) {
    this.sessions.delete(session)
    this.reported.delete(session)
    // 新 opencode 会话的上下文是空的，人格要重新注入。
    this.personas.delete(session)
    void this.#storageSet(`session:${session}`, null)
  }

  async #alive(sessionID) {
    try {
      const session = await this.ctx.session.get({ sessionID })
      const directory = session?.directory ?? session?.data?.directory
      // A session whose recorded directory no longer exists cannot be resumed:
      // opencode fails every prompt with "Directory not found". Treat it as
      // dead so #session creates a fresh one under the current state dir.
      if (typeof directory === "string" && !fs.existsSync(directory)) return false
      return true
    } catch {
      return false
    }
  }

  /**
   * `session.wait` 只表示「这一轮结束了」，模型/网关失败也会正常 resolve。
   * 这里再查一次会话状态，把失败按轮次失败报给桥，避免桥把「没有回复」当成成功。
   *
   * v2 的 `setup(ctx)` 只暴露 `tool` / `session` / `storage` / `app`，没有
   * `session.error` 订阅入口；插件拿不到上游 body。提示只陈述失败结果并指向
   * opencode server 日志，不猜测额度、鉴权或模型不可用。
   */
  async #assertTurnSucceeded(sessionID) {
    let session
    try {
      session = await this.ctx.session.get({ sessionID })
    } catch {
      // 状态查不到不改判，保持原来的成功语义，别把正常轮次误判成失败。
      return
    }
    const outcome = session?.outcome ?? session?.data?.outcome
    if (outcome === undefined || outcome === "succeeded") return
    throw new Error(t("opencode.turn_outcome_failed", { sessionId: sessionID, outcome }))
  }

  async #storageGet(key) {
    try {
      const value = await this.ctx.storage?.get?.(key)
      return value ?? null
    } catch {
      return null
    }
  }

  async #storageSet(key, value) {
    try {
      if (value === null) await this.ctx.storage?.remove?.(key)
      else await this.ctx.storage?.set?.(key, value)
    } catch {
      // 记不住就退回内存里那份，不值得把这一轮弄挂。
    }
  }
}

// ---------------------------------------------------------------------------
// 工具面：名字和参数跟桥上的工具一一对应
// ---------------------------------------------------------------------------

async function callBridge(link, name, args, options = {}) {
  let wire
  try {
    wire = normalizeToolArgs(name, args)
  } catch (error) {
    // 本地就能看出来的错，别浪费一个来回。
    throw new Error(error.message)
  }
  try {
    const raw = await link.call(name, wire, options)
    // capabilities scope=tool 的完整 schema 在 harness 本地；桥只返回权限判定。
    const result =
      name === "capabilities" && wire.scope === "tool" && wire.name
        ? withLocalSchema(wire.name, raw)
        : raw
    // 空结果只代表平台没给数据，返回机器可读的成功标记，不编人话。
    if (result === null || result === undefined) return JSON.stringify({ ok: true })
    return typeof result === "string"
      ? result
      : JSON.stringify(result, null, 2)
  } catch (error) {
    const code = error instanceof BridgeError ? error.code : "internal"
    throw new Error(`${name} failed (${code}): ${error.message}`)
  }
}


/** 按 surface 选 opencode 工具定义；dsh 与 opencode 共用 shared/tools.mjs 的选择逻辑。 */
export function toolSpecsForSurface(surface = {}) {
  return toolsForSurface(surface).map(({ name, description, schema }) => ({
    name,
    description,
    input: schema,
  }))
}


// 工具面只在 shared/tools.mjs 维护一份（dsh 也从那里拿）；这里只把 `schema` 改名成
// opencode 认的 `input`，并按桥下发的 surface 选常驻 + 可见子集。以前这里是一份手抄
// 的副本，实测已经漂移（共享版加了 fetch_records，这边没有），别再抄第二份。

function readConfig(ctx) {
  const options = ctx?.options ?? {}
  return {
    endpoint:
      (typeof options.endpoint === "string" && options.endpoint.trim()) ||
      process.env.ACB_ENDPOINT?.trim() ||
      "127.0.0.1:7700",
    token:
      (typeof options.token === "string" && options.token.trim()) ||
      process.env.ACB_TOKEN?.trim() ||
      "",
    harness:
      (typeof options.harness === "string" && options.harness.trim()) ||
      process.env.ACB_HARNESS?.trim() ||
      "opencode",
    session:
      (typeof options.session === "string" && options.session.trim()) || "",
    logFile:
      (typeof options.logFile === "string" && options.logFile.trim()) ||
      process.env.ACB_OPENCODE_LOG?.trim() ||
      "",
  }
}

function makeLogger(ctx, logFile) {
  const line = `[${SERVICE}]`
  let ready = false
  return (level, message) => {
    // 固定写一份到 stderr：实测 2.0.22 在 `serve --stdio` 下 `app.log` 不落盘
    // （opencode.log 里搜不到任何 service= 的行），只靠它等于把诊断信息扔掉。
    // stdout 一个字都不能碰——那是 RPC 通道，写进去会让 opencode 报 socket 意外关闭。
    try {
      console.error(`${line} ${level} ${message}`)
    } catch {
      // 连 stderr 都不可用就算了，日志绝不能把插件弄挂。
    }
    // opencode 没有 web，插件自己的文件日志是「它到底在干什么」最直接的窗口。
    // 没配 logFile 就不写文件——测试和临时实例不该往仓库里留垃圾。
    try {
      if (logFile) {
        if (!ready) {
          fs.mkdirSync(path.dirname(logFile), { recursive: true })
          ready = true
        }
        fs.appendFileSync(
          logFile,
          `${new Date().toISOString()} ${level} ${message}\n`,
        )
      }
    } catch {
      // 写不了文件也不能影响轮次。
    }
    const app = ctx?.app
    if (typeof app?.log === "function") {
      Promise.resolve(
        app.log({ body: { service: SERVICE, level, message } }),
      ).catch(() => {})
    }
  }
}
