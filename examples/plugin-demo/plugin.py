#!/usr/bin/env python3
"""示例插件：stdio 上的行 JSON-RPC。

只依赖 Python 标准库，演示插件平台要求的最小形状：
- `initialize` / `list_tools` / `call_tool` 是载体协议，所有工具插件都要实现；
- `run_action` 是 `manifest.actions` 的执行入口（核心侧的执行器随后接上）。

配置通过环境变量传入：`ACB_PLUGIN_<KEY>`（密钥字段不在其中，走密钥引用）。
"""

import json
import os
import sys


def reply(stdout, request_id, result=None, error=None):
    payload = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        payload["error"] = error
    else:
        payload["result"] = result
    stdout.write(json.dumps(payload, ensure_ascii=False) + "\n")
    stdout.flush()


TOOLS = [
    {
        "name": "demo.lookup",
        "summary": "回显一条示例记录",
        "inputSchema": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "additionalProperties": True,
        },
    }
]


def config_summary():
    return {
        key: value
        for key, value in sorted(os.environ.items())
        if key.startswith("ACB_PLUGIN_")
    }


def call_tool(tool, args):
    if tool == "demo.lookup":
        query = str(args.get("query", ""))
        return {
            "content": [
                {
                    "type": "text",
                    "text": "demo.lookup: " + (query if query else "(empty query)"),
                }
            ]
        }
    return {"content": [{"type": "text", "text": "unknown tool: " + tool}]}


def run_action(name, args):
    if name == "ping":
        return {"pong": True, "config": config_summary(), "args": args}
    return {"error": "unknown action: " + name}


def main():
    stdout = sys.stdout
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            continue
        request_id = request.get("id")
        method = request.get("method", "")
        params = request.get("params") or {}
        if method == "initialize":
            reply(stdout, request_id, {"id": "demo.tools", "version": "0.1.0"})
        elif method == "list_tools":
            reply(stdout, request_id, {"tools": TOOLS})
        elif method == "call_tool":
            reply(
                stdout,
                request_id,
                call_tool(params.get("tool", ""), params.get("args") or {}),
            )
        elif method == "run_action":
            reply(
                stdout,
                request_id,
                run_action(params.get("action", ""), params.get("args") or {}),
            )
        else:
            reply(
                stdout,
                request_id,
                error={"code": -32601, "message": "unknown method: " + method},
            )


if __name__ == "__main__":
    main()
