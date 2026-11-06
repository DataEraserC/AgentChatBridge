// 软链安全的「是否主模块」判定。
//
// 通过 profile/current/... 启动时，process.argv[1] 是软链路径，而 ESM 默认会
// 解析软链，import.meta.url 是真实路径。直接比较两者会把主模块误判为「被 import」，
// 入口脚本于是静默 exit 0（见 drivers 单元的 203/EXEC 循环）。
//
// 这里先把两侧都 realpath 再比较；realpath 失败（例如 argv[1] 已不存在）时退回
// path.resolve，不抛错。
import { realpathSync } from "node:fs"
import { resolve } from "node:path"
import { fileURLToPath } from "node:url"

function canonical(path) {
  try {
    return realpathSync(path)
  } catch {
    return resolve(path)
  }
}

export function isMainModule(moduleUrl, argv = process.argv) {
  if (!argv[1]) return false
  return canonical(argv[1]) === canonical(fileURLToPath(moduleUrl))
}
