/**
 * 查询键：一处定义，避免"改完数据忘了失效哪个"。
 *
 * 约定：键名与后端路径同构（`/api/model-fallbacks` → `queryKeys.modelFallbacks`），
 * 带参数的查询用元组，参数放在末尾。
 */
export const queryKeys = {
  status: ["status"] as const,
  dependencies: ["dependencies"] as const,
  sessions: ["sessions"] as const,
  feedback: ["feedback"] as const,
  plugins: ["plugins"] as const,
  policy: ["policy"] as const,
  models: ["models"] as const,
  modelFallbacks: (session: string | null) => ["model-fallbacks", session] as const,
  modelPenalty: (session: string | null) => ["model-penalty", session] as const,
  config: ["config"] as const,
  /** 密钥名清单（只给名字，不给值）。新增会话要挑一个。 */
  secrets: ["secrets"] as const,
  configToml: ["config-toml"] as const,
  /** 房间枚举按连接分键：换连接要重取。 */
  rooms: (connection: string) => ["rooms", connection] as const,
};
