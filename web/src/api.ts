/**
 * 控制台 API 客户端。
 *
 * 鉴权与旧 console.html 完全一致，不引入新机制：
 * - 进门的 `?token=` 存进 localStorage，之后所有请求带 `Authorization: Bearer`；
 * - 服务端还会种 `acb_console` 的 HttpOnly cookie，刷新后靠它保登录；
 * - localStorage 在沙箱 iframe 里可能被禁，所以存取都包 try/catch。
 *
 * 错误一律变成 `ApiError`：`message` 是给人看的一句话，`detail` 是底层原文，
 * `status` 是 HTTP 状态码。页面只需要决定「显示成错误块」还是「重试」。
 */

const TOKEN_KEY = "acb-console-token";

export class ApiError extends Error {
  readonly status: number;
  readonly detail?: string;

  constructor(message: string, status: number, detail?: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.detail = detail;
  }
}

function readToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) || "";
  } catch {
    return "";
  }
}

function writeToken(token: string): void {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    // 存不进去不算错误：cookie 兜底。
  }
}

/**
 * 启动时从 URL 取一次 token 并把它从地址栏抹掉。
 *
 * 抹地址栏是安全要求，不是美观：`?token=` 会进浏览器历史、Referer、
 * 以及任何被复制出去的链接。旧页面在 `location.replaceState` 里做了同样的事。
 *
 * 返回是否拿到了 token。没拿到时页面应该显示登录引导，而不是直接跳转——
 * 跳转会丢查询串，用户的 `?locale=` 之类的参数也跟着没了。
 */
export function bootstrapToken(): boolean {
  const match = location.search.match(/[?&]token=([^&]*)/);
  if (match) {
    writeToken(decodeURIComponent(match[1]));
    history.replaceState(null, "", location.pathname + location.hash);
  }
  return readToken() !== "";
}

export function hasToken(): boolean {
  return readToken() !== "";
}

/** 把用户提交的令牌存下来（登录表单用）。 */
export function storeToken(token: string): void {
  writeToken(token.trim());
}

/** 清掉本地令牌（退出登录）。cookie 由服务端在下次 401 时自然过期。 */
export function clearToken(): void {
  try {
    localStorage.removeItem(TOKEN_KEY);
  } catch {
    // 忽略：清不掉就交给 cookie 过期。
  }
}

function parseError(status: number, raw: string): ApiError {
  // 服务端错误体是 `{"error": "...", "detail": "..."}`；解析不了就整段当 detail。
  try {
    const parsed = JSON.parse(raw) as { error?: unknown; detail?: unknown };
    const message = typeof parsed.error === "string" ? parsed.error : `HTTP ${status}`;
    const detail = typeof parsed.detail === "string" ? parsed.detail : undefined;
    return new ApiError(message, status, detail);
  } catch {
    return new ApiError(`HTTP ${status}`, status, raw.slice(0, 400));
  }
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  const token = readToken();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  if (init.body !== undefined && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  let response: Response;
  try {
    response = await fetch(path, { ...init, headers, credentials: "same-origin" });
  } catch (error) {
    // 网络层失败（桥没起、被防火墙拦）与 HTTP 错误要分开：前者该提示「桥不可达」。
    throw new ApiError(
      "无法连接桥",
      0,
      error instanceof Error ? error.message : String(error),
    );
  }
  const text = await response.text();
  if (!response.ok) throw parseError(response.status, text);
  if (!text) return undefined as T;
  try {
    return JSON.parse(text) as T;
  } catch {
    throw new ApiError("响应不是 JSON", response.status, text.slice(0, 400));
  }
}

export const api = {
  get<T>(path: string): Promise<T> {
    return request<T>(path);
  },
  post<T>(path: string, body: unknown): Promise<T> {
    return request<T>(path, { method: "POST", body: JSON.stringify(body) });
  },
};
