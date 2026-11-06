// 最小 WebSocket 客户端（node:net 手写），只支持文本帧、ping/pong、正常关闭。
//
// 为什么不引 ws 依赖：v2/v3 的 harness 目录没有 node_modules，部署时也不保证
// 能联网装包；仓库里 dsh 客户端已经是手写实现，这里抽出来给新适配器复用。
//
// 为什么不用全局 WebSocket：node 的全局 WebSocket 走 undici，HTTP 解析器是
// WASM（llhttp），在受限沙箱里会 `WebAssembly.instantiate(): Out of memory`。
// node:net 不碰那条路径，任何 node 版本都能跑。

import net from "node:net"
import { createHash, randomBytes } from "node:crypto"
import { t } from "./i18n.mjs"

/**
 * 一个最小 WebSocket 客户端。`url` 只支持 ws: / wss: 之外的 ws:。
 * 事件通过 `onOpen` / `onMessage` / `onError` / `onClose` 回调，避免再实现一套
 * EventTarget。
 */
export class MiniWebSocket {
  constructor(url, { headers = {}, timeoutMs = 15_000 } = {}) {
    this.url = new URL(url)
    if (this.url.protocol !== "ws:") {
      throw new Error(t("shared.ws_protocol", { protocol: this.url.protocol }))
    }
    this.headers = headers
    this.timeoutMs = timeoutMs
    this.socket = null
    this.buffer = Buffer.alloc(0)
    this.onOpen = () => {}
    this.onMessage = () => {}
    this.onError = () => {}
    this.onClose = () => {}
  }

  get connected() {
    return Boolean(this.socket) && !this.socket.destroyed
  }

  /** 建立连接；resolve 时已完成 HTTP 101 握手。 */
  connect() {
    return new Promise((resolve, reject) => {
      const { hostname, port, pathname, search } = this.url
      // Sec-WebSocket-Key 必须是 16 字节随机数的 base64 编码。
      const key = randomBytes(16).toString("base64")
      const socket = net.createConnection({ host: hostname, port: Number(port || 80) })
      this.socket = socket
      socket.setTimeout(this.timeoutMs)

      const requestLines = [
        `GET ${pathname}${search} HTTP/1.1`,
        `Host: ${hostname}:${port || 80}`,
        "Upgrade: websocket",
        "Connection: Upgrade",
        `Sec-WebSocket-Key: ${key}`,
        "Sec-WebSocket-Version: 13",
        ...Object.entries(this.headers).map(([name, value]) => `${name}: ${value}`),
        "",
        "",
      ].join("\r\n")

      socket.on("connect", () => socket.write(requestLines))
      socket.on("timeout", () => socket.destroy(new Error(t("shared.ws_timeout"))))

      let handshakeDone = false
      socket.on("data", (chunk) => {
        if (!handshakeDone) {
          const text = chunk.toString("latin1")
          const headerEnd = text.indexOf("\r\n\r\n")
          if (headerEnd < 0) {
            this.buffer = Buffer.concat([this.buffer, chunk])
            return
          }
          const headerText = text.slice(0, headerEnd)
          if (!/^HTTP\/1\.1 101/.test(headerText)) {
            reject(new Error(t("shared.ws_handshake_failed", { status: headerText.split("\r\n")[0] })))
            socket.destroy()
            return
          }
          handshakeDone = true
          const rest = chunk.subarray(headerEnd + 4)
          this.buffer = Buffer.concat([this.buffer, rest])
          resolve()
          this.onOpen()
          this.#parseFrames()
        } else {
          this.buffer = Buffer.concat([this.buffer, chunk])
          this.#parseFrames()
        }
      })
      socket.on("error", (error) => {
        if (!handshakeDone) reject(error)
        else this.onError(error)
      })
      socket.on("close", () => this.onClose())
    })
  }

  /** 发一帧文本（客户端帧必须带 mask）。 */
  send(text) {
    if (!this.socket || this.socket.destroyed) return
    const payload = Buffer.from(text, "utf8")
    const mask = randomBytes(4)
    const masked = Buffer.alloc(payload.length)
    for (let i = 0; i < payload.length; i += 1) {
      masked[i] = payload[i] ^ mask[i % 4]
    }
    let header
    if (payload.length < 126) {
      header = Buffer.from([0x81, 0x80 | payload.length])
    } else if (payload.length < 65536) {
      header = Buffer.alloc(4)
      header[0] = 0x81
      header[1] = 0x80 | 126
      header.writeUInt16BE(payload.length, 2)
    } else {
      header = Buffer.alloc(10)
      header[0] = 0x81
      header[1] = 0x80 | 127
      header.writeBigUInt64BE(BigInt(payload.length), 2)
    }
    this.socket.write(Buffer.concat([header, mask, masked]))
  }

  close() {
    try {
      // 关闭帧：0x88 + mask + 空载荷。
      this.socket?.write(Buffer.from([0x88, 0x80, 0, 0, 0, 0]))
    } catch {
      // 已经断开就算了。
    }
    this.socket?.end()
  }

  #parseFrames() {
    for (;;) {
      if (this.buffer.length < 2) return
      const opcode = this.buffer[0] & 0x0f
      let length = this.buffer[1] & 0x7f
      let offset = 2
      if (length === 126) {
        if (this.buffer.length < 4) return
        length = this.buffer.readUInt16BE(2)
        offset = 4
      } else if (length === 127) {
        if (this.buffer.length < 10) return
        length = Number(this.buffer.readBigUInt64BE(2))
        offset = 10
      }
      if (this.buffer.length < offset + length) return
      const payload = this.buffer.subarray(offset, offset + length)
      this.buffer = this.buffer.subarray(offset + length)

      if (opcode === 0x8) {
        this.onClose()
        return
      }
      if (opcode === 0x9) {
        // ping → pong（服务端帧不带 mask）。
        this.socket?.write(Buffer.concat([Buffer.from([0x8a, payload.length]), payload]))
        continue
      }
      if (opcode === 0x1) {
        this.onMessage(payload.toString("utf8"))
      }
      // 其它 opcode（续帧、二进制）这里忽略：协议只用文本帧。
    }
  }
}

/** 校验一段文本是不是合法 WebSocket URL；不合法抛错。 */
export function assertWsUrl(url) {
  const parsed = new URL(url)
  if (parsed.protocol !== "ws:") {
    throw new Error(t("shared.ws_protocol", { protocol: parsed.protocol }))
  }
  return parsed
}

/** 供测试/工具复用的 accept 计算。 */
export function websocketAccept(key) {
  return createHash("sha1")
    .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64")
}
