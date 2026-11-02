-- 匹配 b23.tv / bilibili 链接：后台用 yt-dlp 下载，下载完发回原房间。
--
-- 配置：
--   [[connections.routes]]
--   provider = "lua.b23_video"
--   [connections.routes.options]
--   dir = "/tmp/acb-b23"
--   timeout_ms = 600000
--
-- 路由回调只做匹配（必须快）；下载/转码/上传在后台 task worker 里跑。
-- 命令是 `exec` 任务：它只接受参数数组，不经过 shell，所以链接里的字符不会
-- 变成命令注入。需要本机装了 yt-dlp 和 ffmpeg。
return {
  extension = "route",
  decide = function(options, context)
    local extra = options.extra or {}
    if extra.enabled == false then
      return "pass"
    end

    local url = context.text:match("https?://[%w%.%-/:%?#=&_]+")
    if not url then
      return "pass"
    end
    if not (url:find("b23.tv", 1, true) or url:find("bilibili.com", 1, true)) then
      return "pass"
    end

    local dir = extra.dir or "/tmp/acb-b23"
    return {
      action = "task",
      handler = "exec",
      -- 链接本身记进流水备查，但不叫醒 AI。
      store = true,
      wake = false,
      params = {
        command = {
          "yt-dlp",
          "--no-playlist",
          "--merge-output-format", "mp4",
          "-f", "bv*+ba/b",
          "--print", "after_move:filepath",
          "-o", "%(id)s.%(ext)s",
          url,
        },
        cwd = dir,
        create_dir = true,
        timeout_ms = extra.timeout_ms or 600000,
        send = "video",
      },
    }
  end,
}
