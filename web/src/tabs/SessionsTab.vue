<script setup lang="ts">
/**
 * 会话页：`v-data-table` 直接吃 `/api/sessions` 的形状。
 *
 * 窄屏降级由 Vuetify 的 data-table 自己处理（它按 CSS 宽度收窄列），
 * 不再需要手写 `≤720px` 的"表格转卡片"媒体查询。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { SessionsView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import SubscriptionEditor from "../components/SubscriptionEditor.vue";
import { icons } from "../icons";

const { t } = useI18n();
const client = useQueryClient();
const notice = ref("");

/**
 * 会话控制：`interrupt` 打断当前轮、`reset` 重置映射（断绑）。
 *
 * 后端两条都走 `POST /api/session/:id/<command>`，它转发一个 control 帧给
 * 管理该会话的 harness。没有 harness 在线时返回 409——那说明这个会话现在没有
 * 任何进程在管，得先去把组件拉起来，而不是重试。
 */
const control = useMutation({
  mutationFn: ({ session, command }: { session: string; command: "interrupt" | "reset" }) =>
    api.post<{ ok?: boolean; note?: string; error?: string }>(
      `/api/session/${encodeURIComponent(session)}/${command}`,
      {},
    ),
  onSuccess: async (reply, vars) => {
    if (reply?.ok) {
      notice.value = reply.note ?? t(`console.sessions.${vars.command}Done`);
      await client.invalidateQueries({ queryKey: queryKeys.sessions });
      return;
    }
    notice.value = reply?.error ?? t("console.sessions.controlFailed");
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

const sessions = useQuery({
  queryKey: queryKeys.sessions,
  queryFn: () => api.get<SessionsView>("/api/sessions"),
});

/**
 * 新增会话要挑 harness / persona / token，三者都来自后端，不手写。
 *
 * 为什么必须让用户选 token：`session_op` 的 create 要求 `secret`（密钥名），
 * 没有它配置加载直接报 `session_token_required`。
 */
const config = useQuery({
  queryKey: queryKeys.config,
  queryFn: () =>
    api.get<{
      harnesses?: Array<{ id: string }>;
      personas?: Array<{ id: string }>;
    }>("/api/config"),
});

const secrets = useQuery({
  queryKey: queryKeys.secrets,
  queryFn: () => api.get<{ secrets: Array<{ name: string }> }>("/api/secrets"),
});

const harnessOptions = computed(() =>
  (config.data.value?.harnesses ?? []).map((h) => ({ value: h.id, title: h.id })),
);
const personaOptions = computed(() =>
  (config.data.value?.personas ?? []).map((p) => ({ value: p.id, title: p.id })),
);
const secretOptions = computed(() =>
  (secrets.data.value?.secrets ?? []).map((s) => ({ value: s.name, title: s.name })),
);

/** 新增会话的对话框状态。`open = false` 时内容保留，便于改错重试。 */
const createOpen = ref(false);
const createForm = ref({ id: "", harness: "", persona: "", secret: "", name: "" });

/** 删除确认：先问，再删。会话带订阅与记忆挂载，误删代价高。 */
const deleteTarget = ref<string | null>(null);

/** 配置补丁：一个入口覆盖 `session`(create) 与 `delete`。 */
const patch = useMutation({
  mutationFn: (op: Record<string, unknown>) =>
    api.post<{ ok?: boolean; error?: string; detail?: string; changes?: string[] }>(
      "/api/config/patch",
      { ops: [op] },
    ),
  onSuccess: async (reply) => {
    if (reply?.error) {
      notice.value = reply.detail ? `${reply.error}（${reply.detail}）` : reply.error;
      return;
    }
    notice.value = (reply?.changes ?? []).join("；") || t("console.models.saved");
    createOpen.value = false;
    deleteTarget.value = null;
    await client.invalidateQueries({ queryKey: queryKeys.sessions });
    await client.invalidateQueries({ queryKey: queryKeys.config });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

function submitCreate(): void {
  const form = createForm.value;
  patch.mutate({
    op: "session",
    id: form.id.trim(),
    create: {
      harness: form.harness,
      persona: form.persona,
      secret: form.secret,
      ...(form.name.trim() ? { name: form.name.trim() } : {}),
    },
  });
}

function confirmDelete(): void {
  const id = deleteTarget.value;
  if (!id) return;
  patch.mutate({ op: "delete", section: "session", id });
}

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
  { title: "", key: "actions", sortable: false },
]);
</script>

<template>
  <PageCard title-key="console.sessions.title" tip-key="console.sessions.intro">
    <template #actions>
      <v-btn
        size="small"
        color="primary"
        variant="tonal"
        :prepend-icon="icons.add"
        @click="createOpen = true"
      >
        {{ t("console.sessions.create") }}
      </v-btn>
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
        <template #item.actions="{ item }">
          <v-btn
            icon
            size="x-small"
            variant="text"
            :aria-label="t('console.sessions.interrupt')"
            :loading="control.isPending.value"
            @click.stop="control.mutate({ session: item.id, command: 'interrupt' })"
          >
            <v-icon :icon="icons.stop" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="error"
            :aria-label="t('console.sessions.delete')"
            @click.stop="deleteTarget = item.id"
          >
            <v-icon :icon="icons.remove" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="warning"
            :aria-label="t('console.sessions.reset')"
            :loading="control.isPending.value"
            @click.stop="control.mutate({ session: item.id, command: 'reset' })"
          >
            <v-icon :icon="icons.disconnect" size="small" />
          </v-btn>
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

    <!-- 新增会话：harness / persona / token 全部从后端列，不手写。 -->
    <v-dialog v-model="createOpen" max-width="36rem">
      <v-card>
        <v-card-title>{{ t("console.sessions.createTitle") }}</v-card-title>
        <v-card-text>
          <v-text-field
            v-model="createForm.id"
            :label="t('console.sessions.col.id')"
            :hint="t('console.sessions.createIdHint')"
            persistent-hint
            density="comfortable"
            variant="outlined"
            class="mb-4"
          />
          <v-select
            v-model="createForm.harness"
            :items="harnessOptions"
            item-title="title"
            item-value="value"
            :label="t('console.sessions.col.harness')"
            density="comfortable"
            variant="outlined"
            class="mb-4"
          />
          <v-select
            v-model="createForm.persona"
            :items="personaOptions"
            item-title="title"
            item-value="value"
            :label="t('console.sessions.persona')"
            density="comfortable"
            variant="outlined"
            class="mb-4"
          />
          <v-select
            v-model="createForm.secret"
            :items="secretOptions"
            item-title="title"
            item-value="value"
            :label="t('console.sessions.secret')"
            :hint="t('console.sessions.secretHint')"
            persistent-hint
            density="comfortable"
            variant="outlined"
          />
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="createOpen = false">{{ t("console.feedback.cancel") }}</v-btn>
          <v-btn
            color="primary"
            :loading="patch.isPending.value"
            :disabled="
              !createForm.id.trim() ||
              !createForm.harness ||
              !createForm.persona ||
              !createForm.secret
            "
            @click="submitCreate"
          >
            {{ t("console.sessions.create") }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>

    <!-- 删除会话：先确认。会话带订阅与记忆挂载，误删代价高。 -->
    <v-dialog :model-value="deleteTarget !== null" max-width="30rem" @update:model-value="deleteTarget = null">
      <v-card>
        <v-card-title>{{ t("console.sessions.deleteTitle", { id: deleteTarget ?? "" }) }}</v-card-title>
        <v-card-text class="text-body-2">
          {{ t("console.sessions.deleteWarning") }}
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="deleteTarget = null">{{ t("console.feedback.cancel") }}</v-btn>
          <v-btn color="error" :loading="patch.isPending.value" @click="confirmDelete">
            {{ t("console.sessions.delete") }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </PageCard>
</template>
