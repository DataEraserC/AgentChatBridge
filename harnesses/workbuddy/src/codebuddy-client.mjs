// WorkBuddy / CodeBuddy headless CLI 的进程封装。
//
// 每轮起一个 `node .../cli/dist/codebuddy-headless.js -p ... --output-format
// stream-json`，从 stdout 的 JSON 事件里拿结果，进程退出即这一轮结束。
//
// 为什么不常驻 --serve：CLI 的 `-p + stream-json + --session-id/-r` 是公开文档化
// 的入口，且每轮进程天然隔离；常驻 HTTP/ACP 没有外部控制文档，先不做。
//
// 这个文件只认 CodeBuddy CLI，不认 acb 桥、不认聊天平台，所以能单独测
// （test/driver.test.mjs 用一个假的 headless CLI 验它）。

import { spawn } from "node:child_process"
import fs from "node:fs"
import readline from "node:readline"
import { t } from "../../shared/i18n.mjs"

const STDERR_LIMIT = 8 * 1024

/** CLI 失败。`kind` 是适配器内部分类，`message` 已经可读。 */
export class CodebuddyError extends Error {
  constructor(kind, message, { exitCode = null, signal = null, stderr = "", result = null, sessionId = "" } = {}) {
    super(message)
    this.name = "CodebuddyError"
    this.kind = kind
    this.exitCode = exitCode
    this.signal = signal
    this.stderr = stderr
    this.result = result
    this.sessionId = sessionId
  }
}

/** 这个错误是不是「resume 的旧会话不存在」，可以改成新建会话重试。 */
export function isResumeMissing(error) {
  if (!(error instanceof CodebuddyError)) return false
  if (error.kind === "spawn_failed") return false
  return /no conversation|session[^.]*?(not[ -]?found|unknown|does not exist|missing)|cannot resume|could not resume|no such session/i
    .test(`${error.message}\n${error.stderr}`)
}

/** 一个 WorkBuddy CLI 入口（每次 runTurn 拉起一个子进程）。 */
export class CodebuddyClient {
  constructor({
    cliPath,
    nodePath = process.execPath,
    log = () => {},
    baseEnv = process.env,
    cwd = process.cwd(),
    // 多实例隔离：HOME / CODEBUDDY_CONFIG_DIR / CODEBUDDY_DATA_ROOT 全部指向
    // 实例自己的 state 目录，登录态和会话记录不会串到另一个实例。
    home = "",
    configDir = "",
    dataRoot = "",
  }) {
    if (!cliPath) throw new Error(t("workbuddy.cli_missing", { path: "" }))
    this.cliPath = cliPath
    this.nodePath = nodePath
    this.log = log
    this.baseEnv = baseEnv
    this.cwd = cwd
    this.home = home
    this.configDir = configDir
    this.dataRoot = dataRoot
    this.child = null
    this.runState = null
    /** 最近几轮实际用的参数；测试和排障用。 */
    this.runs = []
  }

  /** 打断当前 CLI 进程。 */
  abort() {
    if (this.runState) this.runState.aborted = true
    this.#kill("SIGINT")
    // SIGINT 不奏效时兜底强杀，别让一轮永远挂着。
    setTimeout(() => this.#kill("SIGKILL"), 5_000).unref?.()
  }

  /**
   * 跑一轮。返回 `{ sessionId, resultText, events, mcpServers }`；
   * 任何非正常结束都抛 {@link CodebuddyError}。
   */
  runTurn({
    prompt,
    sessionId,
    resume = false,
    mcpUrl,
    appendSystemPrompt = "",
    allowedTools = [],
    settings = "",
    permissionMode = "",
    maxTurns = 0,
    tools = null,
    timeoutMs = 5 * 60 * 1000,
  }) {
    if (!mcpUrl) throw new Error(t("workbuddy.mcp_url_missing"))
    const args = buildArgs({
      sessionId,
      resume,
      mcpUrl,
      appendSystemPrompt,
      allowedTools,
      settings,
      permissionMode,
      maxTurns,
      tools,
      prompt,
    })
    this.runs.push({ sessionId, resume, args })
    const runState = { aborted: false, timedOut: false }
    this.runState = runState
    for (const dir of [this.home, this.configDir, this.dataRoot]) {
      if (!dir) continue
      try {
        fs.mkdirSync(dir, { recursive: true })
      } catch (error) {
        throw new Error(t("workbuddy.state_dir_failed", {
          path: dir,
          error: error?.message ?? error,
        }))
      }
    }
    const env = {
      ...this.baseEnv,
      // 多实例：HOME 隔离登录态与会话记录；CODEBUDDY_CONFIG_DIR 是 CLI 自己
      // 用来放 settings/jobs/MCP 配置的目录（默认 ~/.codebuddy）。
      ...(this.home ? { HOME: this.home, USERPROFILE: this.home } : {}),
      ...(this.configDir
        ? { CODEBUDDY_CONFIG_DIR: this.configDir, WORKBUDDY_CONFIG_DIR: this.configDir }
        : {}),
      ...(this.dataRoot ? { CODEBUDDY_DATA_ROOT: this.dataRoot } : {}),
      // 强制走 headless bundle：即使入口是 codebuddy launcher，也不会去
      // require 上游发行物里不存在的 dist/codebuddy.js。
      CODEBUDDY_FORCE_HEADLESS_BUNDLE: "1",
      NO_COLOR: "1",
      FORCE_COLOR: "0",
    }

    return new Promise((resolve, reject) => {
      let child
      try {
        child = spawn(this.nodePath, [this.cliPath, ...args], {
          cwd: this.cwd,
          env,
          stdio: ["ignore", "pipe", "pipe"],
          windowsHide: true,
        })
      } catch (error) {
        reject(new CodebuddyError("spawn_failed", t("workbuddy.spawn_failed", {
          error: error?.message ?? error,
        })))
        return
      }
      this.child = child
      this.log("info", t("workbuddy.cli_start", {
        sessionId,
        resume: resume ? "yes" : "no",
        pid: child.pid ?? "?",
        mcpUrl,
      }))

      const events = []
      const result = { value: null }
      const init = { value: null }
      const errorEvent = { value: null }
      const assistantTexts = []
      let stderr = ""
      let settled = false

      const timer = timeoutMs > 0
        ? setTimeout(() => {
            runState.timedOut = true
            this.log("warn", t("workbuddy.timeout_kill", {
              seconds: Math.round(timeoutMs / 1000),
              sessionId,
            }))
            this.#kill("SIGTERM")
            setTimeout(() => this.#kill("SIGKILL"), 2_000).unref?.()
          }, timeoutMs)
        : null

      const finish = (fn) => {
        if (settled) return
        settled = true
        if (timer) clearTimeout(timer)
        this.child = null
        if (this.runState === runState) this.runState = null
        fn()
      }

      const rl = readline.createInterface({ input: child.stdout, crlfDelay: Infinity })
      rl.on("line", (line) => {
        const text = line.trim()
        if (!text) return
        let event
        try {
          event = JSON.parse(text)
        } catch {
          this.log("debug", t("workbuddy.non_json_line", { text: text.slice(0, 200) }))
          return
        }
        events.push(event)
        const type = event?.type
        if (type === "system" && event.subtype === "init") init.value = event
        else if (type === "result") result.value = event
        else if (type === "assistant") {
          const text = assistantText(event)
          if (text) assistantTexts.push(text)
        } else if (type === "error") {
          errorEvent.value = event
          this.log("warn", t("workbuddy.cli_error_event", {
            error: errorText(event),
          }))
        }
      })

      child.stderr.setEncoding("utf8")
      child.stderr.on("data", (chunk) => {
        if (stderr.length < STDERR_LIMIT) stderr += chunk
      })

      child.on("error", (error) => {
        finish(() => reject(new CodebuddyError("spawn_failed", t("workbuddy.spawn_failed", {
          error: error?.message ?? error,
        }), { stderr: tail(stderr) })))
      })

      child.on("exit", (code, signal) => {
        finish(() => {
          const resultEvent = result.value
          const session = resultEvent?.session_id || init.value?.session_id || sessionId
          if (runState.timedOut) {
            reject(new CodebuddyError(
              "timeout",
              t("workbuddy.timeout", { seconds: Math.round(timeoutMs / 1000) }),
              { exitCode: code, signal, stderr: tail(stderr), result: resultEvent, sessionId: session },
            ))
            return
          }
          if (runState.aborted) {
            reject(new CodebuddyError("aborted", t("workbuddy.aborted"), {
              exitCode: code, signal, stderr: tail(stderr), result: resultEvent, sessionId: session,
            }))
            return
          }

          const mcp = mcpStatus(init.value)
          if (mcp.failed) {
            reject(new CodebuddyError(
              "mcp_unavailable",
              t("workbuddy.mcp_unavailable", { status: mcp.status || "failed" }),
              { exitCode: code, signal, stderr: tail(stderr), result: resultEvent, sessionId: session },
            ))
            return
          }

          const failedResult =
            resultEvent &&
            (resultEvent.is_error === true || /error|failed|interrupted/i.test(String(resultEvent.subtype ?? "")))
          if (failedResult) {
            reject(new CodebuddyError(
              "result_error",
              resultErrorText(resultEvent, stderr),
              { exitCode: code, signal, stderr: tail(stderr), result: resultEvent, sessionId: session },
            ))
            return
          }

          if (code !== 0) {
            reject(new CodebuddyError(
              "exit_error",
              t("workbuddy.cli_exit_error", {
                code: code ?? "?",
                signal: signal ?? "",
                stderr: tail(stderr) || resultEvent?.result || "",
              }),
              { exitCode: code, signal, stderr: tail(stderr), result: resultEvent, sessionId: session },
            ))
            return
          }

          // 实测 CLI 在「resume 的会话不存在」时会发一条 {type:"error"} 然后
          // 以 0 退出，没有 result 事件。这种必须按失败上报，不能当成成功。
          if (!resultEvent && errorEvent.value) {
            reject(new CodebuddyError(
              "result_error",
              errorText(errorEvent.value),
              { exitCode: code, signal, stderr: tail(stderr), result: null, sessionId: session },
            ))
            return
          }

          if (!resultEvent) {
            this.log("warn", t("workbuddy.no_result_event", { sessionId: session }))
          }
          resolve({
            sessionId: session,
            resultText: resultEvent?.result ?? assistantTexts.join("\n"),
            events,
            mcpServers: mcp.servers,
          })
        })
      })
    })
  }

  #kill(signal) {
    const child = this.child
    if (!child || child.exitCode !== null) return
    try {
      child.kill(signal)
    } catch {
      // 进程已经没了。
    }
  }
}

function buildArgs({
  sessionId,
  resume,
  mcpUrl,
  appendSystemPrompt,
  allowedTools,
  settings,
  permissionMode,
  maxTurns,
  tools,
  prompt,
}) {
  const args = ["-p", "--output-format", "stream-json", "--strict-mcp-config"]
  args.push("--mcp-config", JSON.stringify({
    mcpServers: {
      acb: { type: "http", url: mcpUrl },
    },
  }))
  if (appendSystemPrompt) args.push("--append-system-prompt", appendSystemPrompt)
  if (settings) args.push("--settings", settings)
  if (permissionMode) args.push("--permission-mode", permissionMode)
  if (maxTurns > 0) args.push("--max-turns", String(maxTurns))
  // 默认关掉 CLI 的内置工具（Bash/Edit/Read…），只留 MCP 工具：ACB 的会话是
  // 聊天，不需要本地工具，也避免模型用 shell 绕过桥直接敲平台端口。
  if (typeof tools === "string") args.push("--tools", tools)
  if (Array.isArray(allowedTools) && allowedTools.length > 0) {
    args.push("--allowedTools", ...allowedTools)
  }
  if (resume) args.push("-r", sessionId)
  else args.push("--session-id", sessionId)
  // prompt 放最后，避免以 '-' 开头的内容被当成选项。
  args.push(prompt)
  return args
}

function tail(text, limit = 2_000) {
  const value = String(text ?? "").trim()
  return value.length > limit ? `…${value.slice(-limit)}` : value
}

function resultErrorText(result, stderr) {
  const errors = Array.isArray(result?.errors) ? result.errors.find(Boolean) : ""
  const text = String(
    result?.result ?? result?.error?.message ?? errors ?? result?.subtype ?? "",
  ).trim()
  if (text) return text
  const err = tail(stderr)
  return err || t("workbuddy.result_error")
}

function errorText(event) {
  const value = event?.error ?? event?.message ?? event
  if (typeof value === "string") return value
  if (value && typeof value.message === "string") return value.message
  try {
    return JSON.stringify(value).slice(0, 500)
  } catch {
    return String(value)
  }
}

/** `assistant` 事件里的文本；不同 CLI 版本可能是 content 数组或纯字符串。 */
function assistantText(event) {
  const content = event?.message?.content
  if (typeof content === "string") return content.trim()
  if (!Array.isArray(content)) return ""
  return content
    .map((block) => {
      if (typeof block === "string") return block
      if (block?.type === "text" && typeof block.text === "string") return block.text
      return ""
    })
    .filter(Boolean)
    .join("")
    .trim()
}

/** 从 init 事件里看 acb MCP server 有没有连上。 */
function mcpStatus(init) {
  const servers = init?.mcp_servers ?? init?.mcpServers
  if (!servers) return { failed: false, status: "", servers: [] }
  let entry = null
  if (Array.isArray(servers)) {
    entry = servers.find((item) => item?.name === "acb") ?? null
  } else if (typeof servers === "object") {
    entry = servers.acb ?? null
  }
  if (!entry) return { failed: false, status: "", servers }
  const status = String(entry.status ?? entry.state ?? "").trim()
  const failed = /fail|error|disconnect|unavailable/i.test(status)
  return { failed, status, servers }
}
