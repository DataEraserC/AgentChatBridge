-- 管理员 /stop：提前结束命令所在房间的轮次。
--
-- 配置：
--   [[connections.routes]]
--   provider = "lua.stop_command"
--   [connections.routes.options]
--   commands = ["/stop"]
--   scope = "room"          -- room / connection
--   admin_only = true
--
-- 只认连接上 admin_qqs 里的号；非管理员发 /stop 只是普通消息。
-- 支持群里常见的 /stop@机器人名，只认第一个词，后面跟字也照样停。
return {
  extension = "route",
  decide = function(options, context)
    local extra = options.extra or {}
    if extra.admin_only ~= false and not context.is_admin then
      return "pass"
    end

    local first = context.text:match("^%s*(%S+)")
    if not first then
      return "pass"
    end
    local command = first:match("^([^@]+)@") or first

    local commands = extra.commands or { "/stop" }
    for _, name in ipairs(commands) do
      if command:lower() == name:lower() then
        return {
          action = "interrupt",
          scope = extra.scope or "room",
        }
      end
    end
    return "pass"
  end,
}
