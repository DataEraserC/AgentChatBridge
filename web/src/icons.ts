/**
 * 图标：把旧控制台用的 `mdi-xxx` 名映射到 `@mdi/js` 的 SVG 路径。
 *
 * **为什么需要这一层**：`vuetify.ts` 用的是 `vuetify/iconsets/mdi-svg`（不加载
 * 图标字体，CSP 友好）。这个图标集**不吃字符串名**——`<v-icon icon="mdi-arrow-up">`
 * 会把 `mdi-arrow-up` 原样写进 `<path d="mdi-arrow-up">`，浏览器画不出任何东西。
 *
 * 症状是最难查的那种：按钮**存在、可点、量得到 20×20、aria-label 也对**，
 * 只是屏幕上空白一片。2026-11-17 用户报「模型页看不到删除和调顺序按钮」就是
 * 这个——三个按钮一直都在。（当时我以为是 `@mdi/js` 没装，装了之后发现路径
 * 还是空的，因为代码里根本没引用它。）
 *
 * 用法：
 *   import { icons } from "../icons";
 *   <v-icon :icon="icons.arrowUp" />
 *
 * 新增图标时：在这里加一行，然后 `icon` 指向它。**不要**再写字符串名。
 */

import {
  mdiArrowDown,
  mdiArrowUp,
  mdiClose,
  mdiDeleteOutline,
  mdiFileEyeOutline,
  mdiInformationOutline,
  mdiKeyOutline,
  mdiRefresh,
  mdiSwapHorizontal,
} from "@mdi/js";

/** 项目用到的图标。键名按用途取，不照抄 MDI 名字。 */
export const icons = {
  /** 刷新/重取。 */
  refresh: mdiRefresh,
  /** 「更多信息」的 ⓘ。 */
  info: mdiInformationOutline,
  /** 新旧控制台互跳。 */
  swap: mdiSwapHorizontal,
  /** 密钥/凭据。 */
  key: mdiKeyOutline,
  /** 只读/查看。 */
  view: mdiFileEyeOutline,
  /** 删除。 */
  remove: mdiDeleteOutline,
  /** 关闭/移除行。 */
  close: mdiClose,
  /** 上移。 */
  arrowUp: mdiArrowUp,
  /** 下移。 */
  arrowDown: mdiArrowDown,
} as const;

export type IconName = keyof typeof icons;
