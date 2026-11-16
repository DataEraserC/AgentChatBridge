/**
 * 服务端状态：TanStack Query。
 *
 * 为什么不再手写 `useResource`（上一版是自己写的 120 行，含竞态保护、轮询、
 * 可见性判断）：那些正是这个库解决的问题，而且它做得更全——请求去重、
 * 缓存失效、窗口重新聚焦刷新、重试退避、`enabled` 条件查询、开发期 Devtools。
 * 手写版每加一个页面就要再想一遍"缓存多久、什么时候失效"，那是在重造轮子。
 *
 * 约定：
 *   - 每个查询一个 `queryKey`，改完数据用 `invalidateQueries` 而不是手工重取；
 *   - `staleTime` 短（5s）：运维页面看到旧状态比多点一次请求更糟；
 *   - 401 不重试：令牌错重试只会更慢地失败。
 */

import { QueryClient, VueQueryPlugin } from "@tanstack/vue-query";
import { ApiError } from "./api";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // 运维页面要"现在是什么"，不是"5 分钟前是什么"。
      staleTime: 5_000,
      // 控制台不需要无限保留。
      gcTime: 5 * 60_000,
      refetchOnWindowFocus: true,
      retry: (failureCount, error) => {
        // 鉴权失败重试没有意义（令牌不会自己变对）。
        if (error instanceof ApiError && (error.status === 401 || error.status === 403)) {
          return false;
        }
        return failureCount < 2;
      },
    },
  },
});

export { VueQueryPlugin };
