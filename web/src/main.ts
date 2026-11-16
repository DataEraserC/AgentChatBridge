import { createApp } from "vue";
import App from "./App.vue";
import { router } from "./router";
import { vuetify } from "./plugins/vuetify";
import * as components from "vuetify/components";
import * as directives from "vuetify/directives";
import "vuetify/styles";
import { queryClient, VueQueryPlugin } from "./query";
import { bootstrapToken, clearToken, hasToken } from "./api";
import { useI18n } from "./i18n";

/**
 * 入口。
 *
 * 顺序有讲究：
 *   1. 先取 token（会把 `?token=` 从地址栏抹掉，越早越好）；
 *   2. 再加载业务词条（页面文案全靠它，失败也要继续——宁可显示 key 也不白屏）；
 *   3. 最后挂载，并装上 Vuetify / 路由 / 查询缓存。
 */
async function boot(): Promise<void> {
  const authed = bootstrapToken();
  if (!authed) clearToken();

  const { load } = useI18n();
  await load();

  const app = createApp(App);
  // 全量注册：见 vite.config.ts 里关闭 autoImport 的理由。
  for (const [name, component] of Object.entries(components)) {
    app.component(name, component);
  }
  // 各指令的 binding 类型互不相同，逐个注册会让 TS 去解一个不属于它的联合类型。
  // 注册是运行时行为，这里显式断言一次，注释说明为什么。
  for (const [name, directive] of Object.entries(directives)) {
    app.directive(name, directive as Parameters<typeof app.directive>[1]);
  }
  app
    .use(router)
    .use(vuetify)
    .use(VueQueryPlugin, { queryClient })
    .mount("#app");
}

void boot();

// 登录状态在别的标签页变化时同步。
window.addEventListener("storage", (event) => {
  if (event.key === "acb-console-token" && !hasToken()) location.reload();
});
