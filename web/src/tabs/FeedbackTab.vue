<script setup lang="ts">
/**
 * 反馈页：列表 + 回复 + 改状态。
 *
 * 回复会作为事件通知原会话（agent 能看到），所以它是个**有外部效果的写操作**：
 * 走弹窗、确认后才提交，不做行内即点即发。改状态只动库里那一列，风险低，
 * 给下拉直接切。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { FeedbacksView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import { icons } from "../icons";

const { t } = useI18n();
const client = useQueryClient();
const notice = ref("");

const feedback = useQuery({
  queryKey: queryKeys.feedback,
  queryFn: () => api.get<FeedbacksView>("/api/feedback"),
});

const rows = computed(() => feedback.data.value?.feedback ?? []);

const headers = computed(() => [
  { title: t("console.feedback.col.id"), key: "id" },
  { title: t("console.feedback.col.title"), key: "title" },
  { title: t("console.feedback.col.status"), key: "status" },
  { title: t("console.feedback.col.last"), key: "created_at" },
  { title: "", key: "actions", sortable: false },
]);

/** 回复弹窗状态。`id = null` 表示关着。 */
const replyTo = ref<number | null>(null);
const replyText = ref("");

const REPLY = useMutation({
  mutationFn: ({ id, text }: { id: number; text: string }) =>
    api.post<{ ok?: boolean; error?: string }>(`/api/feedback/${id}/reply`, { text }),
  onSuccess: async (result) => {
    if (result?.error) {
      notice.value = result.error;
      return;
    }
    notice.value = t("console.feedback.replied");
    replyTo.value = null;
    replyText.value = "";
    await client.invalidateQueries({ queryKey: queryKeys.feedback });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

const STATUS = useMutation({
  mutationFn: ({ id, status }: { id: number; status: string }) =>
    api.post<{ ok?: boolean; error?: string }>(`/api/feedback/${id}/status`, { status }),
  onSuccess: async (result) => {
    if (result?.error) {
      notice.value = result.error;
      return;
    }
    await client.invalidateQueries({ queryKey: queryKeys.feedback });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

const STATUS_OPTIONS = computed(() => [
  { value: "open", title: t("console.feedback.status.open") },
  { value: "acked", title: t("console.feedback.status.acked") },
  { value: "resolved", title: t("console.feedback.status.resolved") },
  // 后端还接受 `wontfix`（见 `set_feedback` 的白名单），别漏掉它——
  // 少了这一项，用户在想「这条不修」时只能瞎选一个别的状态。
  { value: "wontfix", title: t("console.feedback.status.wontfix") },
]);

function beginReply(id: number): void {
  replyTo.value = id;
  replyText.value = "";
}

function submitReply(): void {
  const id = replyTo.value;
  const text = replyText.value.trim();
  if (id == null || !text) return;
  REPLY.mutate({ id, text });
}

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
        :prepend-icon="icons.refresh"
        :loading="feedback.isFetching.value"
        @click="feedback.refetch()"
      >
        {{ t("console.feedback.refresh") }}
      </v-btn>
    </template>

    <v-alert
      v-if="notice"
      type="info"
      variant="tonal"
      density="compact"
      closable
      class="mb-3"
      :text="notice"
      @click:close="notice = ''"
    />
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
        <template #item.actions="{ item }">
          <v-select
            :model-value="item.status"
            :items="STATUS_OPTIONS"
            item-title="title"
            item-value="value"
            density="compact"
            variant="plain"
            hide-details
            style="max-width: 8rem"
            :loading="STATUS.isPending.value"
            @update:model-value="STATUS.mutate({ id: item.id, status: String($event) })"
          />
          <v-btn
            icon
            size="x-small"
            variant="text"
            :aria-label="t('console.feedback.reply')"
            @click.stop="beginReply(item.id)"
          >
            <v-icon :icon="icons.info" size="small" />
          </v-btn>
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

    <!-- 回复是发给原会话的事件，走弹窗确认，不做行内即点即发。 -->
    <v-dialog :model-value="replyTo !== null" max-width="40rem" @update:model-value="replyTo = null">
      <v-card>
        <v-card-title>{{ t("console.feedback.replyTitle", { id: replyTo ?? 0 }) }}</v-card-title>
        <v-card-text>
          <v-textarea
            v-model="replyText"
            :label="t('console.feedback.replyLabel')"
            :hint="t('console.feedback.replyHint')"
            persistent-hint
            rows="4"
            density="comfortable"
            variant="outlined"
          />
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="replyTo = null">{{ t("console.feedback.cancel") }}</v-btn>
          <v-btn
            color="primary"
            :loading="REPLY.isPending.value"
            :disabled="!replyText.trim()"
            @click="submitReply"
          >
            {{ t("console.feedback.send") }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </PageCard>
</template>
