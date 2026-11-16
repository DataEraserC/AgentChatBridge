/**
 * 业务词条：全部来自服务端 `GET /api/i18n?locale=`。
 *
 * 分工要清楚，否则会写出两套翻译：
 *   - **Vuetify 组件的内部文案**（"没有数据"、分页、"每页条数"）由 Vuetify 自己的
 *     locale 包负责，见 `plugins/vuetify.ts`；
 *   - **我们自己的界面文案**（导航、卡片标题、解释）由这里负责。
 * 两者都随 `locale` 一起切换，但互不接管。
 *
 * 为什么不打包词条进 bundle：服务端已有 1500+ 条经校对的词条，复制一份必然漂移；
 * 而且同一条错误在 JSON 响应里是服务端语言、在页面上是前端语言时，会出现混排。
 */

import { computed, ref, type ComputedRef } from "vue";
import { api, ApiError } from "./api";
import type { I18nCatalog } from "./types";

export const SUPPORTED = ["zh-CN", "en"] as const;
export type Locale = (typeof SUPPORTED)[number];

const LOCALE_KEY = "acb-console-locale";

const messages = ref<Record<string, string>>({});
const locale = ref<Locale>("zh-CN");
const ready = ref(false);

function stored(): string | null {
  try {
    return localStorage.getItem(LOCALE_KEY);
  } catch {
    return null;
  }
}

function initial(): Locale {
  const fromUrl = new URLSearchParams(location.search).get("locale");
  const candidate = fromUrl || stored() || "";
  return (SUPPORTED as readonly string[]).includes(candidate) ? (candidate as Locale) : "zh-CN";
}

/**
 * 渲染 `{name}` 占位。语法与服务端 `acb_i18n::msg!` 一致。
 *
 * 未提供值的占位**原样保留**（显示成 `{error}`），让漏传变成可见问题，
 * 而不是静默空串。
 */
function format(template: string, params?: Record<string, string | number>): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (whole, key: string) =>
    key in params ? String(params[key]) : whole,
  );
}

/**
 * 取一条业务词条。
 *
 * 故意做成"读响应式 locale 的普通函数"而不是必须 `useI18n()` 的 hook：
 * 模板里直接写 `t('console.nav.status')` 最省事，依赖靠读 `locale.value` 建立，
 * 换语言时组件会重新渲染。
 */
export function t(key: string, params?: Record<string, string | number>): string {
  void locale.value;
  const template = messages.value[key];
  if (template === undefined) return key;
  return format(template, params);
}

export function useI18n(): {
  t: typeof t;
  locale: ComputedRef<Locale>;
  supported: readonly Locale[];
  ready: ComputedRef<boolean>;
  load: (next?: Locale) => Promise<void>;
  change: (next: Locale) => Promise<void>;
} {
  return {
    t,
    locale: computed(() => locale.value),
    supported: SUPPORTED,
    ready: computed(() => ready.value),
    load,
    change,
  };
}

export async function load(next?: Locale): Promise<void> {
  const target = next ?? initial();
  try {
    const catalog = await api.get<I18nCatalog>(`/api/i18n?locale=${encodeURIComponent(target)}`);
    locale.value = (SUPPORTED as readonly string[]).includes(catalog.locale)
      ? (catalog.locale as Locale)
      : target;
    messages.value = catalog.messages ?? {};
  } catch (error) {
    // 拿不到词条不是致命错误：结构照常渲染，文案退化成 key。
    // 但要在控制台留原因，否则"页面全是点号"会像前端坏了。
    if (error instanceof ApiError) {
      console.error(`i18n 加载失败（HTTP ${error.status}）：${error.message}`);
    }
    locale.value = target;
    messages.value = {};
  }
  ready.value = true;
  document.documentElement.lang = locale.value;
}

export async function change(next: Locale): Promise<void> {
  if (!SUPPORTED.includes(next) || next === locale.value) return;
  try {
    localStorage.setItem(LOCALE_KEY, next);
  } catch {
    // 存不进去就只在本次会话生效。
  }
  await load(next);
}
