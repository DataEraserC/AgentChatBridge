<script setup lang="ts">
/**
 * 应用外壳。
 *
 * 布局、导航、主题与语言切换全部交给 Vuetify：v-app / v-app-bar / v-tabs /
 * v-btn / v-select / v-tooltip 自带布局、横向滚动、键盘与 aria。
 * 这个文件只剩三件事：哪些 tab、点了去哪、主题怎么存。没有布局 CSS。
 */

import { computed, ref, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { clearToken, hasToken } from "./api";
import { useI18n, type Locale } from "./i18n";
import { TABS } from "./router";
import { vuetifyLocale } from "./plugins/vuetify";
import LoginGate from "./LoginGate.vue";
import { icons } from "./icons";

const { t, locale, change } = useI18n();
const route = useRoute();
const router = useRouter();

const authed = ref(hasToken());

const THEME_KEY = "acb-console-theme";
type ThemeChoice = "system" | "light" | "dark";

function readTheme(): ThemeChoice {
  try {
    const saved = localStorage.getItem(THEME_KEY);
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // 存储被禁：跟随系统。
  }
  return "system";
}

const themeChoice = ref<ThemeChoice>(readTheme());
/** Vuetify 只有 dark/light 两个主题名，system 在这里解析。 */
const vuetifyTheme = ref<"dark" | "light">("dark");

function applyTheme(next: ThemeChoice): void {
  themeChoice.value = next;
  try {
    if (next === "system") localStorage.removeItem(THEME_KEY);
    else localStorage.setItem(THEME_KEY, next);
  } catch {
    // 本次会话内仍然生效。
  }
  const dark =
    next === "dark" ||
    (next === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  vuetifyTheme.value = dark ? "dark" : "light";
  document.documentElement.dataset.theme = vuetifyTheme.value;
}

applyTheme(themeChoice.value);

window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (themeChoice.value === "system") applyTheme("system");
});

const activeTab = computed(() => route.path);

function go(path: string): void {
  void router.push(path);
}

watch(
  () => route.path,
  (path) => {
    const tab = TABS.find((item) => item.path === path);
    if (tab) document.title = `${t(tab.key)} · acb-bridge`;
  },
  { immediate: true },
);

/**
 * 跳到旧版控制台（`GET /`）。
 *
 * 为什么可以无缝跳：两套前端用**同一个** localStorage key
 * （`acb-console-token`）和**同一个** cookie（`acb_console`，`Path=/`），
 * 所以跳过去不用重新登录。这一点在改动任何一边的 token 逻辑时要同步核对，
 * 否则跳转就会变成"跳过去还要输一次令牌"。
 *
 * 用 `location.href` 而不是 router：旧版是另一个页面，不是本 SPA 的路由。
 */
function openLegacyConsole(): void {
  location.href = "/";
}

function signOut(): void {
  clearToken();
  authed.value = false;
}

const localeItems = computed(() =>
  ["zh-CN", "en"].map((code) => ({
    value: code,
    title: code === "zh-CN" ? t("console.ui.locale.zh") : t("console.ui.locale.en"),
  })),
);

const themeItems = computed(() => [
  { value: "system", title: t("console.ui.theme.system") },
  { value: "light", title: t("console.ui.theme.light") },
  { value: "dark", title: t("console.ui.theme.dark") },
]);

async function onLocale(value: Locale): Promise<void> {
  await change(value);
}

const vLocale = computed(() => vuetifyLocale(locale.value));
</script>

<template>
  <LoginGate v-if="!authed" @authed="authed = true" />

  <v-app v-else :theme="vuetifyTheme" :locale="vLocale">
    <v-app-bar flat density="comfortable" :elevation="0">
      <v-app-bar-title>
        <span class="text-body-1 font-weight-medium">{{ t("console.ui.page.title") }}</span>
      </v-app-bar-title>

      <v-btn icon variant="text" size="small" :aria-label="t('console.ui.info.more')">
        <v-icon :icon="icons.info" />
        <v-tooltip activator="parent" location="bottom" max-width="420">
          {{ t("console.ui.header.tip") }}
        </v-tooltip>
      </v-btn>

      <v-spacer />

      <v-select
        :model-value="themeChoice"
        :items="themeItems"
        item-title="title"
        item-value="value"
        density="compact"
        variant="outlined"
        hide-details
        class="topbar-select"
        :aria-label="t('console.ui.theme.label')"
        @update:model-value="applyTheme($event as ThemeChoice)"
      />
      <v-select
        :model-value="locale"
        :items="localeItems"
        item-title="title"
        item-value="value"
        density="compact"
        variant="outlined"
        hide-details
        class="topbar-select"
        :aria-label="t('console.ui.locale.label')"
        @update:model-value="onLocale($event as Locale)"
      />
      <v-btn
        variant="text"
        size="small"
        :prepend-icon="icons.swap"
        :title="t('console.ui.toLegacyTip')"
        @click="openLegacyConsole"
      >
        {{ t("console.ui.toLegacy") }}
      </v-btn>
      <v-btn variant="text" size="small" @click="signOut">
        {{ t("console.ui.logout") }}
      </v-btn>

    </v-app-bar>

    <v-main>
      <!-- 导航放在 v-main 里：v-app-bar 是 fixed 的，而 Vuetify 只给 v-main
           加顶部偏移；导航放在 v-main 之外会被顶栏盖住（实测踩过）。 -->
      <v-tabs
        :model-value="activeTab"
        density="comfortable"
        show-arrows
        align-tabs="start"
        border="b"
        @update:model-value="go($event as string)"
      >
        <v-tab v-for="tab in TABS" :key="tab.path" :value="tab.path">
          {{ t(tab.key) }}
        </v-tab>
      </v-tabs>
      <v-container fluid class="pa-4 page">
        <router-view />
      </v-container>
    </v-main>
  </v-app>
</template>

<style scoped>
.topbar-select {
  max-width: 10rem;
  margin-inline-end: 0.5rem;
}
.page {
  max-width: 1200px;
}
</style>
