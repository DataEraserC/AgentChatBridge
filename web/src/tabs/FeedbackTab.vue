<script setup lang="ts">
/**
 * 反馈页：只读列表。回复入口未做——回复会作为事件通知原会话，
 * 写错话的代价比看错状态大，等交互语义定下来再加。
 */

import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { FeedbacksView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";

const { t } = useI18n();

const feedback = useQuery({
  queryKey: queryKeys.feedback,
  queryFn: () => api.get<FeedbacksView>("/api/feedback"),
});

const rows = computed(() => feedback.data.value?.feedbacks ?? []);

const headers = computed(() => [
  { title: t("console.feedback.col.id"), key: "id" },
  { title: t("console.feedback.col.title"), key: "title" },
  { title: t("console.feedback.col.status"), key: "status" },
  { title: t("console.feedback.col.last"), key: "created_at" },
]);

/** 状态 → chip 颜色。未知状态给中性色，不假装它是某种已知状态。 */
function statusColor(status: string): string {
  if (status === "open") return "primary";
  if (status === "resolved") return "success";
  if (status === "acked") return "info";
  return "default";
}

function when(at: number | undefined): string {
  return at ? new Date(at).toLocaleString() : t("console.deps.none");
}
</script>

<template>
  <PageCard title-key="console.ui.feedback.title" tip-key="console.feedback.intro">
    <template #actions>
      <v-chip v-if="rows.length" size="small" variant="tonal">
        {{ t("console.feedback.count", { n: rows.length }) }}
      </v-chip>
      <v-btn
        size="small"
        variant="text"
        prepend-icon="mdi-refresh"
        :loading="feedback.isFetching.value"
        @click="feedback.refetch()"
      >
        {{ t("console.feedback.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="feedback" :empty="!rows.length" empty-key="console.feedback.empty">
      <v-data-table
        :headers="headers"
        :items="rows"
        item-value="id"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.feedback.empty')"
      >
        <template #item.id="{ item }">
          <code class="text-caption">#{{ item.id }}</code>
        </template>
        <template #item.title="{ item }">
          {{ item.title || t("console.deps.none") }}
        </template>
        <template #item.status="{ item }">
          <v-chip :color="statusColor(item.status)" size="small" variant="tonal">
            {{ t("console.ui.state." + item.status) }}
          </v-chip>
        </template>
        <template #item.created_at="{ item }">
          <span class="text-caption text-medium-emphasis">{{ when(item.created_at) }}</span>
        </template>
      </v-data-table>
    </QueryState>
  </PageCard>
</template>
