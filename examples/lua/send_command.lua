-- 管理员 /send：消息直接由插件发出去，不叫醒 AI。
--
-- 配置：
--   [[connections.routes]]
--   provider = "lua.send_command"
--   [connections.routes.options]
--   command = "/send"
--   admin_only = true
--
-- 例：/send 明天下午三点开会
-- 命令本身不写流水；发出去的消息走正常的发送钩子和文件中继。
return {
  extension = "route",
  decide = function(options, context)
    local extra = options.extra or {}
    if extra.admin_only ~= false and not context.is_admin then
      return "pass"
    end

    local first, rest = context.text:match("^%s*(%S+)%s*(.*)$")
    if not first then
      return "pass"
    end
    local command = first:match("^([^@]+)@") or first
    if command:lower() ~= (extra.command or "/send"):lower() then
      return "pass"
    end
    if rest == "" then
      return "pass"
    end

    return {
      action = "send",
      text = rest,
      store = false,
      wake = false,
    }
  end,
}
