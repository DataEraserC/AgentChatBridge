/**
 * Vuetify 实例。
 *
 * 三件与"别手写"直接相关的事都在这里：
 *   1. **主题**：`themes.light/dark` 直接把 Vuetify 的颜色槽接到我们已有的设计令牌上
 *      （深色 `#14161a` 那套），页面不再自己写一套色板。
 *   2. **i18n**：Vuetify 自己的组件文案（"没有数据"、分页、"每页条数"）走它的
 *      locale 包；我们业务的词条仍走服务端。两套互不干扰——这是 Vuetify 的
 *      `locale` 只管组件内部字符串，不接管 `t()`。
 *   3. **显示断点**：Vuetify 的 `useDisplay` 提供 xs/sm/md/lg 判定，表格降级用它，
 *      不再手写 `@media (max-width: 720px)`。
 */

import { createVuetify } from "vuetify";
import { aliases, mdi } from "vuetify/iconsets/mdi-svg";
import { en, zhHans } from "vuetify/locale";

/** 与旧控制台一致的设计令牌（深色为主）。 */
const DARK = {
  background: "#14161a",
  surface: "#1b1e24",
  "surface-bright": "#22262e",
  "surface-variant": "#22262e",
  "on-surface": "#dfe3ea",
  "on-surface-variant": "#939aa6",
  "on-background": "#dfe3ea",
  primary: "#4f9cf9",
  "primary-darken-1": "#3b7fcc",
  secondary: "#939aa6",
  success: "#5cc07f",
  warning: "#d9a03f",
  error: "#dd6b6b",
  info: "#4f9cf9",
  outline: "#3a414b",
  "outline-variant": "#2f343d",
};

const LIGHT = {
  background: "#f5f6f8",
  surface: "#ffffff",
  "surface-bright": "#eef1f5",
  "surface-variant": "#eef1f5",
  "on-surface": "#2a3039",
  "on-surface-variant": "#667085",
  "on-background": "#2a3039",
  primary: "#2563eb",
  "primary-darken-1": "#1d4fd0",
  secondary: "#667085",
  success: "#1c7f48",
  warning: "#8a5d05",
  error: "#b3261e",
  info: "#2563eb",
  outline: "#c6cdd7",
  "outline-variant": "#dde2e9",
};

export const vuetify = createVuetify({
  theme: {
    defaultTheme: "dark",
    themes: {
      dark: { dark: true, colors: DARK },
      light: { dark: false, colors: LIGHT },
    },
  },
  // 组件内文案（"无数据"、"每页 N 条"）跟随界面语言。
  locale: {
    locale: "zhHans",
    fallback: "en",
    messages: { zhHans, en },
  },
  // 图标：用 MDI 的 SVG 集合（不额外加载字体，CSP 友好）。
  icons: { defaultSet: "mdi", aliases, sets: { mdi } },
  defaults: {
    // 全局默认：紧凑一些，运维页面要能一屏看更多。
    VBtn: { variant: "flat", density: "comfortable" },
    VTextField: { variant: "outlined", density: "comfortable", hideDetails: "auto" },
    VSelect: { variant: "outlined", density: "comfortable", hideDetails: "auto" },
    VSwitch: { density: "comfortable", hideDetails: "auto" },
    VCard: { variant: "outlined" },
    VDataTable: { density: "comfortable" },
  },
});

/** Vuetify locale 名 ↔ 我们服务端 locale 名。 */
export function vuetifyLocale(locale: string): string {
  return locale === "en" ? "en" : "zhHans";
}
