<script setup lang="ts">
/**
 * 会话订阅编辑。
 *
 * 这是房间发现能力的**消费端**，也是用户最初诉求的落点：
 * 「不再从日志里翻房间号」——能枚举就下拉，不能就手输。
 *
 * 保存走 `/api/config/patch` 的 `subscription` op（对象类操作写库，按轮次边界
 * 生效）。增删各一条 op，不做「整份覆盖」：覆盖会把并发改动一起抹掉。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import RoomPicker from "./RoomPicker.vue";

const props = defineProps<{
  sessionId: string;
  /** 已订阅的房间串（`qq:group/1` 形态）。 */
  subscriptions: string[];
}>();

const { t } = useI18n();
const client = useQueryClient();

const notice = ref("");
const connection = ref("");
const draftRoom = ref("");

/** 连接列表从 `/api/config` 拿：那里已经按敏感字段遮过一遍。 */
const config = useQuery({
  queryKey: queryKeys.config,
  queryFn: () => api.get<{ connections?: Array<{ id: string }> }>("/api/config"),
});

const connections = computed(() =>
  (config.data.value?.connections ?? []).map((item) => ({ value: item.id, title: item.id })),
);

const save = useMutation({
  mutationFn: (op: Record<string, unknown>) =>
    api.post<{ ok?: boolean; error?: string; detail?: string }>("/api/config/patch", { ops: [op] }),
  onSuccess: async (reply) => {
    if (reply?.error) {
      notice.value = reply.detail ? `${reply.error}（${reply.detail}）` : reply.error;
      return;
    }
    notice.value = t("console.models.saved");
    await client.invalidateQueries({ queryKey: queryKeys.sessions });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

/** 从 `qq:group/1` 里取出连接名与房间段，拼成 patch 要的形状。 */
function add(): void {
  const room = draftRoom.value.trim();
  if (!room) return;
  const parts = room.split(":");
  if (parts.length < 2) {
    notice.value = t("console.rooms.badFormat");
    return;
  }
  const [conn, ...rest] = parts;
  save.mutate({
    op: "subscription",
    session: props.sessionId,
    connection: conn,
    room: rest.join(":"),
  });
  draftRoom.value = "";
}

function remove(room: string): void {
  save.mutate({
    op: "subscription",
    session: props.sessionId,
    room,
    remove: true,
  });
}
</script>

<template>
  <div>
    <div v-for="room in subscriptions" :key="room" class="d-flex align-center ga-2 mb-1">
      <code class="text-caption">{{ room }}</code>
      <v-btn
        icon
        size="x-small"
        variant="text"
        color="error"
        :aria-label="t('console.rooms.remove')"
        :disabled="save.isPending.value"
        @click="remove(room)"
      >
        <v-icon icon="mdi-close" size="small" />
      </v-btn>
    </div>

    <v-select
      v-model="connection"
      :items="connections"
      item-title="title"
      item-value="value"
      :label="t('console.rooms.connection')"
      density="comfortable"
      variant="outlined"
      hide-details
      class="mt-3"
      style="max-width: 18rem"
    />

    <div class="d-flex ga-2 mt-2 flex-wrap align-start">
      <div style="flex: 1 1 22rem; min-width: 0">
        <RoomPicker
          v-model="draftRoom"
          :connection="connection"
          :label="t('console.rooms.add')"
        />
      </div>
      <v-btn
        color="primary"
        :disabled="save.isPending.value || !draftRoom.trim()"
        @click="add"
      >
        {{ t("console.rooms.add") }}
      </v-btn>
    </div>

    <div v-if="notice" class="text-caption text-medium-emphasis mt-2">{{ notice }}</div>
  </div>
</template>
