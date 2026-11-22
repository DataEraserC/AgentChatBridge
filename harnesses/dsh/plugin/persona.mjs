// acb 的人格段：把会话的 persona 注册成 dsh 的 system prompt 段落。
//
// 为什么是插件而不是拼进 prompt：dsh 的 system prompt 由 `ctx.systemPrompt`
// 集中组装（`packages/core/system-prompt`），插件注册的段落才是真正的 system
// 层；拼在用户消息里的「人格」优先级低，而且每轮重复一遍。
//
// 为什么 persona 从**文件**读，而不是走桥：`PromptSection.text` 只能是同步的
// `string | ((context) => string)`，`AssembleContext` 又不带会话标识（只有不透明
// 的 `scope`）。同步读一个本地文件正好合规，也让「persona 改了立刻生效」成立
// ——桥每次投递前覆写该文件即可，不需要重新生成 patch 或重载 dsh。
//
// 每个 preset 挂一个实例，`config.session` 标明自己是谁。

import { readFileSync } from "node:fs"

/** persona 段在 dsh 里的排序名。用仓库预留的 persona 前缀位。 */
const SECTION = "deployment:persona-prefix"

/**
 * 读 persona 文本。文件不存在或为空时返回空串——注册空段等于没注册，dsh 不会
 * 因此报错，这也是 dsh 重启后桥还没投递过时的正常状态。
 *
 * 读失败**不抛**：装配 system prompt 期间抛错会让整轮起不来，而人格缺失只是
 * 少一段说明。出错留给日志。
 */
function readPersona(path) {
  try {
    return readFileSync(path, "utf8").trim()
  } catch {
    return ""
  }
}

export function apply(ctx, config = {}) {
  const path = config.file
  if (typeof path !== "string" || !path) {
    // 配置漏了就不注册，但要说出来——静默跳过会让人以为 persona 生效了。
    ctx.logger?.warn?.("acb persona: 缺少 config.file，不注册人格段")
    return
  }
  const order = ctx.systemPrompt.getSectionOrder(SECTION)
  ctx.systemPrompt.section({
    name: SECTION,
    order,
    text: () => readPersona(path),
    // persona 是自由文本，里面出现 `{{...}}` 不该被当变量求值。
    interpolate: false,
  })
}

export default { name: "acb-persona", apply }
