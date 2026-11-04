#!/usr/bin/env node
// 最小外置工具插件：stdio 上的行 JSON-RPC。只暴露一个 echo 工具。
// 用 `node examples/process-tool-echo/plugin.js` 启动（由 bridge 负责拉起）。

import readline from "node:readline"

const rl = readline.createInterface({ input: process.stdin })
const tools = [
  {
    name: "echo",
    summary: "echo the arguments back",
    inputSchema: { type: "object", additionalProperties: true },
  },
]

rl.on("line", (line) => {
  let request
  try {
    request = JSON.parse(line)
  } catch {
    return
  }
  const id = request.id ?? null
  if (request.method === "initialize") {
    reply(id, { id: "echo.tools", version: "0.1.0" })
  } else if (request.method === "list_tools") {
    reply(id, { tools })
  } else if (request.method === "call_tool") {
    if (request.params?.name !== "echo") {
      replyError(id, -32601, `unknown tool: ${request.params?.name}`)
      return
    }
    reply(id, { result: request.params?.arguments ?? {} })
  } else if (request.method === "shutdown") {
    reply(id, {})
    process.exit(0)
  } else {
    replyError(id, -32601, `unknown method: ${request.method}`)
  }
})

function reply(id, result) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`)
}

function replyError(id, code, message) {
  process.stdout.write(
    `${JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } })}\n`,
  )
}
