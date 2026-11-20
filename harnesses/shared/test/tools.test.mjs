// 工具 schema 的形状约束。只验「每个定义都自带方言声明」这一条——
// 具体字段对不对由各自的调用方和桥的校验负责。

import { test } from "node:test"
import assert from "node:assert/strict"

import { TOOLS } from "../tools.mjs"

// 手写 JSON Schema 容易漏 `$schema`。下游（dsh 用 ajv）拿不到方言就无法确定
// 按哪版规范处理，模型那边会看到「schema 不可用」，只能靠猜参数形态
// （2026-11-20：模型把 `ids` 数组写成 JSON 字符串，卡了 92 次重投）。
test("每个工具的 schema 都声明了 JSON Schema 方言", () => {
  assert.ok(TOOLS.length > 0, "工具表不该为空")
  const missing = TOOLS.filter(
    (tool) => tool.schema?.$schema !== "http://json-schema.org/draft-07/schema#",
  ).map((tool) => tool.name)
  assert.deepEqual(missing, [], `这些工具缺 $schema：${missing.join(", ")}`)
})
