// 两个 harness 的可见面一致性：同一份 surface → 同一组工具名。
//
// dsh 用 MCP tools/list，opencode 用 ctx.tool.transform；两者都从
// shared/tools.mjs 的 toolsForSurface 选，测试同时盯住两边的适配函数。

import test from "node:test"
import assert from "node:assert/strict"

import { RESIDENT_TOOL_NAMES, bootstrapSurface } from "../../shared/tools.mjs"
import { surface } from "../../opencode/test/fake-bridge.mjs"
import { toolDefsForSurface } from "../src/driver.mjs"
import { toolSpecsForSurface } from "../../opencode/plugin/index.js"

function names(list) {
  return list.map((tool) => tool.name)
}

test("MCP 启动面就把目录层注册出来，长尾与按需工具不会永远是 unknown", () => {
  const boot = names(toolDefsForSurface(bootstrapSurface()))
  for (const tool of RESIDENT_TOOL_NAMES) {
    assert.ok(boot.includes(tool), `常驻工具 ${tool} 启动时必须可见`)
  }
  for (const tool of ["plugin_tools", "plugin_tool_call", "get_history", "fetch_records"]) {
    assert.ok(boot.includes(tool), `${tool} 必须在启动面里注册`)
  }
  assert.equal(boot.includes("extensions_list"), false, "管理员专属工具不进启动面")
})

test("同一会话同一场所：dsh 与 opencode 选出同一批工具", () => {
  const fixture = surface()
  const dsh = names(toolDefsForSurface(fixture))
  const opencode = names(toolSpecsForSurface(fixture))

  assert.deepEqual(dsh, opencode, "两个 harness 的可见工具名必须一致");
  for (const tool of RESIDENT_TOOL_NAMES) {
    assert.ok(dsh.includes(tool), `常驻工具 ${tool} 必须在两边都可见`);
  }
  assert.ok(dsh.includes("notes"), "visible 里的长尾工具要出现");
  assert.equal(dsh.includes("extensions_list"), false, "hidden 工具两边都不能出现");
});

test("surface 收窄时两个 harness 一起收窄", () => {
  const fixture = surface({
    catalog: surface().catalog.filter((entry) => entry.tool === "notes"),
  });
  const dsh = names(toolDefsForSurface(fixture));
  const opencode = names(toolSpecsForSurface(fixture));

  assert.deepEqual(dsh, opencode);
  assert.ok(dsh.includes("notes"));
  assert.equal(dsh.includes("memory_git"), false, "不在 visible 的长尾不能偷偷出现");
});
