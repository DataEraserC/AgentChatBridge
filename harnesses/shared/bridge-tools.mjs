// 新适配器共用的「桥工具面」小工具。
//
// dsh 的 driver.mjs 里已经有一份同样的逻辑；那个文件是既有实现，不改。这里把
// 只读、无状态的部分抽出来给 lobsterai / workbuddy 复用：
//   - toolDefsForSurface：按桥下发的 surface 选本轮 MCP 工具定义；
//   - callBridge：把 MCP tools/call 转成带当前 ticket 的 tool_call；
//   - overflowTools：投递溢出时临时注入回看工具。

import { BridgeError, normalizeToolArgs } from "../opencode/plugin/acb-wire.mjs"
import {
  ON_DEMAND_TOOL_NAMES,
  toolsForSurface,
  withLocalSchema,
} from "./tools.mjs"

/** 这一轮是否需要临时注入回看工具。 */
export function overflowTools(delivery) {
  const more = Number(delivery?.more ?? 0)
  return delivery?.omitted || more > 0 ? ON_DEMAND_TOOL_NAMES : []
}

/** 按 surface 选 MCP 工具定义（inputSchema 字段）。 */
export function toolDefsForSurface(surface = {}, options = {}) {
  return toolsForSurface(surface, options).map(({ name, description, schema }) => ({
    name,
    description,
    inputSchema: schema,
  }))
}

/**
 * 调一个桥工具，返回给模型看的字符串。
 *
 * 参数先过本地归一化（send_message 的 elements/text 糖、ack_messages 的整数校验），
 * 再原样交给 BridgeLink；ticket 由 BridgeLink 自己带，适配器不生成、不旁路。
 */
export async function callBridge(link, name, args, options = {}) {
  let wire
  try {
    wire = normalizeToolArgs(name, args)
  } catch (error) {
    throw new Error(error.message)
  }
  try {
    const raw = await link.call(name, wire, options)
    // capabilities scope=tool 的完整 schema 在 harness 本地；桥只返回权限判定。
    const result =
      name === "capabilities" && wire.scope === "tool" && wire.name
        ? withLocalSchema(wire.name, raw)
        : raw
    // 桥返回空 = 平台没有可返回的数据（例如撤回成功）；给机器可读的成功标记。
    if (result === null || result === undefined) return JSON.stringify({ ok: true })
    return typeof result === "string" ? result : JSON.stringify(result, null, 2)
  } catch (error) {
    const code = error instanceof BridgeError ? error.code : "internal"
    throw new Error(`${name} failed (${code}): ${error.message}`)
  }
}
