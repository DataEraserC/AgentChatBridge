<script setup lang="ts">
/**
 * 概览页。
 *
 * 指标卡片用 Vuetify 的 grid + `v-card`，依赖表用 `v-data-table`
 * （自带表头、空态、分页、响应式）。这页没有一行手写表格或卡片样式。
 */

import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { DependenciesView, DependencyEntry, StatusView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import { icons } from "../icons";

const { t } = useI18n();

const status = useQuery({
  queryKey: queryKeys.status,
  queryFn: () => api.get<StatusView>("/api/status"),
  refetchInterval: 15_000,
});

const deps = useQuery({
  queryKey: queryKeys.dependencies,
  queryFn: () => api.get<DependenciesView>("/api/dependencies"),
  refetchInterval: 15_000,
});

/** 指标卡：值统一转成字符串，Vuetify 的卡片只负责排版。 */
const stats = computed(() => {
  const data = status.data.value;
  if (!data) return [];
  return [
    { key: "console.ui.status.connections", value: String(data.connections) },
    { key: "console.ui.status.sessions", value: String(data.sessions) },
    { key: "console.ui.status.harnesses", value: String(data.harnesses) },
    // 后端给的是会话 id 数组，页面上要的是"几个在跑"。
    { key: "console.ui.status.busySessions", value: String(data.busy_sessions.length) },
    { key: "console.ui.status.queuedTurns", value: String(data.queued_turns) },
    { key: "console.ui.status.configVersion", value: String(data.config_version) },
    // 构建身份（`<版本>+<提交>.<提交日期>`）：分辨「这台桥跑的是哪次构建」。
    // 放在最后一行：它是最不常看、但排障时最需要的一条。
    { key: "console.ui.status.buildVersion", value: String(data.version ?? "-") },
  ];
});

const depHeaders = computed(() => [
  { title: t("console.deps.col.id"), key: "id" },
  { title: t("console.deps.col.state"), key: "state" },
  { title: t("console.deps.col.lastOkAt"), key: "lastOkAt" },
  { title: t("console.deps.col.attempts"), key: "attempts" },
  { title: t("console.deps.col.lastError"), key: "detail" },
]);

/** 状态 → Vuetify 的 chip 颜色。 */
function stateColor(state: DependencyEntry["state"]): string {
  if (state === "up") return "success";
  if (state === "down") return "error";
  return "default";
}

function when(at: number | undefined): string {
  return at ? new Date(at).toLocaleString() : t("console.deps.never");
}

const downCount = computed(
  () => (deps.data.value?.dependencies ?? []).filter((entry) => entry.state === "down").length,
);

async function refresh(): Promise<void> {
  await Promise.all([status.refetch(), deps.refetch()]);
}
</script>

<template>
  <PageCard title-key="console.ui.overview.title" tip-key="console.ui.status.reloadTip">
    <template #actions>
      <v-btn
        size="small"
        variant="text"
        :prepend-icon="icons.refresh"
        :loading="status.isFetching.value"
        @click="refresh"
      >
        {{ t("console.deps.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="status">
      <v-row dense>
        <v-col v-for="item in stats" :key="item.key" cols="6" sm="4" md="2">
          <v-card variant="tonal" class="pa-3 h-100">
            <div class="text-caption text-medium-emphasis">{{ t(item.key) }}</div>
            <div class="text-h5 font-weight-medium mt-1">{{ item.value }}</div>
          </v-card>
        </v-col>
      </v-row>
    </QueryState>
  </PageCard>

  <PageCard title-key="console.deps.title" tip-key="console.deps.intro">
    <template #actions>
      <v-chip v-if="downCount" color="error" size="small" variant="tonal">
        {{ t("console.deps.downCount", { count: downCount }) }}
      </v-chip>
      <v-btn
        size="small"
        variant="text"
        :prepend-icon="icons.refresh"
        :loading="deps.isFetching.value"
        @click="deps.refetch()"
      >
        {{ t("console.deps.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="deps" :empty="!(deps.data.value?.dependencies ?? []).length" empty-key="console.deps.empty">
      <v-data-table
        :headers="depHeaders"
        :items="deps.data.value?.dependencies ?? []"
        item-value="id"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.deps.empty')"
      >
        <template #item.id="{ item }">
          <code class="text-caption">{{ item.id }}</code>
        </template>
        <template #item.state="{ item }">
          <v-chip :color="stateColor(item.state)" size="small" variant="tonal">
            {{ t("console.deps.state." + item.state) }}
          </v-chip>
        </template>
        <template #item.lastOkAt="{ item }">
          <span class="text-caption text-medium-emphasis">{{ when(item.lastOkAt) }}</span>
        </template>
        <template #item.attempts="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ item.attempts ?? t("console.deps.none") }}
          </span>
        </template>
        <template #item.detail="{ item }">
          <span class="text-caption text-medium-emphasis">{{ item.detail || t("console.deps.none") }}</span>
        </template>
      </v-data-table>
    </QueryState>
  </PageCard>
</template>
