/**
 * 路由：hash 模式。
 *
 * 为什么 hash 而不是 history 模式：服务端的静态路由只有 `/next` 前缀
 * （`crates/bridge/src/console_web.rs` 的 `is_next_ui_path`），history 模式的
 * 深链（`/next/sessions`）会落到「不是资源就回入口页」那条分支——现在能工作，
 * 但那依赖服务端做 SPA fallback。hash 不产生额外请求，规则更少、更难出错。
 *
 * 路由的另一个作用是**懒加载边界**：每个页面单独一个 chunk。构建时它们被
 * 内联进 `app.js`（`inlineDynamicImports`），所以省的不是请求数，而是
 * 首屏不必付另外六页的代码与依赖。
 */

import { createRouter, createWebHashHistory } from "vue-router";

/** 导航顺序稳定：概览在最前，配置在最后。 */
export const TABS = [
  { path: "/status", name: "status", key: "console.nav.status" },
  { path: "/sessions", name: "sessions", key: "console.nav.sessions" },
  { path: "/feedback", name: "feedback", key: "console.nav.feedback" },
  { path: "/plugins", name: "plugins", key: "console.nav.plugins" },
  { path: "/policy", name: "policy", key: "console.nav.policy" },
  { path: "/models", name: "models", key: "console.nav.models" },
  { path: "/config", name: "config", key: "console.nav.config" },
] as const;

export const router = createRouter({
  history: createWebHashHistory(),
  routes: [
    { path: "/", redirect: "/status" },
    {
      path: "/status",
      name: "status",
      component: () => import("./tabs/StatusTab.vue"),
    },
    {
      path: "/sessions",
      name: "sessions",
      component: () => import("./tabs/SessionsTab.vue"),
    },
    {
      path: "/feedback",
      name: "feedback",
      component: () => import("./tabs/FeedbackTab.vue"),
    },
    {
      path: "/plugins",
      name: "plugins",
      component: () => import("./tabs/PluginsTab.vue"),
    },
    {
      path: "/policy",
      name: "policy",
      component: () => import("./tabs/PolicyTab.vue"),
    },
    {
      path: "/models",
      name: "models",
      component: () => import("./tabs/ModelsTab.vue"),
    },
    {
      path: "/config",
      name: "config",
      component: () => import("./tabs/ConfigTab.vue"),
    },
    // 未知路径回概览，而不是白屏。
    { path: "/:pathMatch(.*)*", redirect: "/status" },
  ],
});
