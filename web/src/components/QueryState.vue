<script setup lang="ts">
/**
 * 三态占位：加载中 / 空 / 出错。
 *
 * 上一版是手写的 109 行（spinner、图标、可展开细节、三套样式）。现在只剩把
 * TanStack Query 的状态映射到 Vuetify 的 `v-progress-linear` 与 `v-alert`：
 * 进度条、告警样式、可折叠细节全部由组件库提供。
 *
 * 用法：
 *   <QueryState :query="sessions" empty-key="console.sessions.empty">
 *     <template #default="{ data }">…</template>
 *   </QueryState>
 */

import { computed } from "vue";
import type { UseQueryReturnType } from "@tanstack/vue-query";
import { useI18n } from "../i18n";

const props = withDefaults(
  defineProps<{
    /** TanStack Query 的返回对象（`useQuery(...)`）。 */
    query: UseQueryReturnType<unknown, unknown>;
    /** 空数据时的 i18n key。 */
    emptyKey?: string;
    /** 数据是否为空（由调用方判断，因为"空"是业务语义）。 */
    empty?: boolean;
  }>(),
  { empty: false },
);

const { t } = useI18n();

const errorMessage = computed(() => {
  const error = props.query.error.value;
  if (!error) return "";
  if (error instanceof Error) return error.message;
  return String(error);
});

const errorDetail = computed(() => {
  const error = props.query.error.value as { detail?: string } | null;
  return error?.detail ?? "";
});
</script>

<template>
  <div v-if="query.isPending.value">
    <v-progress-linear indeterminate color="primary" class="my-4" />
    <p class="text-body-2 text-medium-emphasis text-center mb-4">
      {{ t("console.ui.state.loading") }}
    </p>
  </div>

  <v-alert v-else-if="query.isError.value" type="error" variant="tonal" density="comfortable">
    {{ errorMessage }}
    <details v-if="errorDetail" class="mt-2 text-caption">
      <summary>{{ t("console.ui.error.detail") }}</summary>
      <div class="text-mono break-all">{{ errorDetail }}</div>
    </details>
  </v-alert>

  <v-alert v-else-if="empty" type="info" variant="tonal" density="comfortable">
    {{ emptyKey ? t(emptyKey) : t("console.ui.state.empty") }}
  </v-alert>

  <slot v-else />
</template>
