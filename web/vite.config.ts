import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";
import vuetify from "vite-plugin-vuetify";

/**
 * 产物固定文件名（`app.js` / `app.css`），因为 Rust 侧在编译期就要知道嵌哪几个
 * 文件——带内容哈希的产物它发现不了。
 *
 * `base: "/next/"` 必须与 `crates/bridge/src/console_web.rs` 的 `NEXT_UI_PREFIX`
 * 一致：不设时 Vite 会写 `/app.js`，浏览器就去请求根路径（那是旧页面），白屏。
 * 这条一致性由 `index_html_is_the_built_entry_point` 测试看守。
 */
export default defineConfig({
  plugins: [
    vue(),
    // 关掉 autoImport：它靠模板标签名静态分析来按需引入组件与样式，
    // 对 <v-app-bar-title> 这类多段标签会漏（实测该组件 CSS 没进产物，
    // 页面布局塌成一列）。本控制台体积不是约束，全量引入最不容易坏。
    vuetify({ autoImport: false }),
  ],
  base: "/next/",
  build: {
    outDir: "../crates/bridge/src/console_web",
    emptyOutDir: true,
    target: "es2020",
    // 单文件：七个页面都在首屏可达，拆包只增加请求数。
    rollupOptions: {
      output: {
        entryFileNames: "app.js",
        chunkFileNames: "app.[name].js",
        assetFileNames: "app[extname]",
        inlineDynamicImports: true,
      },
    },
    // 嵌进二进制的产物不该带源码映射。
    sourcemap: false,
    // Vuetify + JSONForms 的体积是已知的（实测 234 kB gzip），不要为它报警。
    chunkSizeWarningLimit: 1200,
  },
  // Vitest：动态 schema 表单与 API 层的单测用 jsdom。
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
