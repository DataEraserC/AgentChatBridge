<script setup lang="ts">
/**
 * 会话页：`v-data-table` 直接吃 `/api/sessions` 的形状。
 *
 * 窄屏降级由 Vuetify 的 data-table 自己处理（它按 CSS 宽度收窄列），
 * 不再需要手写 `≤720px` 的"表格转卡片"媒体查询。
 */

import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { SessionsView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import SubscriptionEditor from "../components/SubscriptionEditor.vue";
import { icons } from "../icons";

const { t } = useI18n();

const sessions = useQuery({
  queryKey: queryKeys.sessions,
  queryFn: () => api.get<SessionsView>("/api/sessions"),
});

const rows = computed(() =>
  (sessions.data.value?.sessions ?? []).map((session) => ({
    ...session,
    // 表格里要"几个房间"，不是数组本身。
    roomCount: session.subscriptions?.length ?? 0,
    // 后端已聚合好，直接用 `count`，不再自己 reduce（形状是对象不是数组）。
    stuck: session.stuck_unread?.count ?? 0,
  })),
);

const headers = computed(() => [
  { title: t("console.sessions.col.id"), key: "id" },
  { title: t("console.sessions.col.harness"), key: "harness" },
  { title: t("console.sessions.col.model"), key: "model" },
  { title: t("console.sessions.col.rooms"), key: "roomCount" },
  { title: t("console.sessions.col.state"), key: "state" },
]);
</script>

<template>
  <PageCard title-key="console.sessions.title" tip-key="console.sessions.intro">
    <template #actions>
      <v-chip v-if="rows.length" size="small" variant="tonal">{{ rows.length }}</v-chip>
      <v-btn
        size="small"
        variant="text"
        :prepend-icon="icons.refresh"
        :loading="sessions.isFetching.value"
        @click="sessions.refetch()"
      >
        {{ t("console.sessions.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="sessions" :empty="!rows.length" empty-key="console.sessions.empty">
      <v-data-table
        :headers="headers"
        :items="rows"
        item-value="id"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.sessions.empty')"
        show-expand
      >
        <!-- 展开行：编辑这个会话的订阅。房间能枚举就下拉，不能就手输。 -->
        <template #expanded-row="{ columns, item }">
          <tr>
            <td :colspan="columns.length" class="pa-4">
              <div class="text-subtitle-2 mb-2">{{ t("console.rooms.editTitle") }}</div>
              <SubscriptionEditor
                :session-id="item.id"
                :subscriptions="item.subscriptions ?? []"
              />
            </td>
          </tr>
        </template>
        <template #item.id="{ item }">
          <code class="text-caption">{{ item.id }}</code>
          <span v-if="item.name && item.name !== item.id" class="text-caption text-medium-emphasis">
            · {{ item.name }}
          </span>
        </template>
        <template #item.model="{ item }">
          <code class="text-caption">{{ item.model || t("console.deps.none") }}</code>
        </template>
        <template #item.roomCount="{ item }">
          <v-tooltip location="top" max-width="320">
            <template #activator="{ props }">
              <span v-bind="props" class="text-caption">
                {{ item.roomCount || t("console.sessions.noRooms") }}
              </span>
            </template>
            <div v-for="sub in item.subscriptions" :key="sub">{{ sub }}</div>
          </v-tooltip>
        </template>
        <template #item.state="{ item }">
          <v-chip v-if="item.busy" color="primary" size="small" variant="tonal" class="me-1">
            {{ t("console.sessions.busy") }}
          </v-chip>
          <v-chip v-else size="small" variant="tonal" class="me-1">
            {{ t("console.sessions.idle") }}
          </v-chip>
          <v-chip v-if="item.stuck" color="warning" size="small" variant="tonal">
            {{ t("console.sessions.stuckShort", { count: item.stuck }) }}
          </v-chip>
        </template>
      </v-data-table>
    </QueryState>
  </PageCard>
</template>
