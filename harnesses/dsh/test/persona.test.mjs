// acb-persona 插件：把文件内容注册成 dsh 的 system prompt 段落。
//
// 只验我们自己的逻辑（注册什么、读哪个文件、文件没了怎么办），
// dsh 的装配与渲染由它自己的测试负责。

import { test } from "node:test"
import assert from "node:assert/strict"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"

import { apply } from "../plugin/persona.mjs"

/** 最小的假 ctx：只提供插件确实用到的那两样。 */
function fakeCtx() {
  const sections = []
  const warnings = []
  return {
    sections,
    warnings,
    systemPrompt: {
      // dsh 的排序由仓库集中分配；这里只要一个稳定的数。
      getSectionOrder: () => 1500,
      section: (spec) => {
        sections.push(spec)
        return () => {}
      },
    },
    logger: { warn: (message) => warnings.push(message) },
  }
}

function withDir(fn) {
  const dir = mkdtempSync(join(tmpdir(), "acb-persona-"))
  try {
    return fn(dir)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

test("注册 persona 前缀段，文本从文件同步读取", () => {
  withDir((dir) => {
    const file = join(dir, "s1.txt")
    writeFileSync(file, "你是测试人格。\n", "utf8")
    const ctx = fakeCtx()
    apply(ctx, { file })

    assert.equal(ctx.sections.length, 1)
    const spec = ctx.sections[0]
    assert.equal(spec.name, "deployment:persona-prefix")
    assert.equal(spec.interpolate, false, "人格是自由文本，不该被当模板求值")
    // `text` 必须是函数：桥重写文件后，下一次装配要拿到新内容。
    assert.equal(typeof spec.text, "function")
    assert.equal(spec.text(), "你是测试人格。")
  })
})

test("文件改了，下一次装配读到新值（不需要重新注册）", () => {
  withDir((dir) => {
    const file = join(dir, "s1.txt")
    writeFileSync(file, "旧人格", "utf8")
    const ctx = fakeCtx()
    apply(ctx, { file })
    const spec = ctx.sections[0]

    assert.equal(spec.text(), "旧人格")
    writeFileSync(file, "新人格", "utf8")
    assert.equal(spec.text(), "新人格", "persona 改了要立刻生效")
  })
})

test("文件不存在时返回空串，装配不报错", () => {
  withDir((dir) => {
    const ctx = fakeCtx()
    apply(ctx, { file: join(dir, "missing.txt") })
    // 空段等于没内容；读失败不该让整轮起不来。
    assert.equal(ctx.sections[0].text(), "")
  })
})

test("缺少 config.file 时不注册，并留下警告", () => {
  const ctx = fakeCtx()
  apply(ctx, {})
  assert.equal(ctx.sections.length, 0)
  assert.equal(ctx.warnings.length, 1, "静默跳过会让人以为人格生效了")
})
