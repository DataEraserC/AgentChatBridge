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
import { icons } from "../icons";

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
/** 房间选择器暴露的校验状态：`unknown` 时禁止提交（ADR-0035）。 */
const picker = ref<{ verifyState: { state: string; detail: string } } | null>(null);

/**
 * 连接变了：清掉已选的房间并重新问一遍候选。
 *
 * 为什么不用 `v-model`：`v-select` 的 `:items` 是 `computed` 出来的对象数组，
 * 配 `v-model` 时实测**不更新**——`connection` 一直是空字符串，于是
 * `RoomPicker` 的 `enabled` 恒为 false、`/api/rooms` 从不发出，下拉里只剩父级
 * 残留的菜单项。换成显式 `@update:model-value` 才可靠。
 */
function onConnectionChange(value: string | null): void {
  const next = (value ?? "").trim();
  if (next === connection.value) return;
  connection.value = next;
  // 换连接后原来的房间大概率不属于新连接，清掉免得提交一个错配的组合。
  draftRoom.value = "";
  picker.value = null;
}

/** 手输了一个库中不存在的房间：报错、不提交、不留痕。 */
const blocked = computed(() => picker.value?.verifyState.state === "unknown");

/** 连接列表从 `/api/config` 拿：那里已经按敏感字段遮过一遍。 */
const config = useQuery({
  queryKey: queryKeys.config,
  queryFn: () =>
    api.get<{
      connections?: Array<{ id: string }>;
      // `/api/config` 的 sessions 是**完整结构**（每个订阅带 wake 策略），
      // 与 `/api/sessions` 那个扁平化视图不同。
      sessions?: Array<{ id: string; subscriptions?: unknown[] }>;
    }>("/api/config"),
});

const connections = computed(() =>
  (config.data.value?.connections ?? []).map((item) => ({ value: item.id, title: item.id })),
);

/** 唤醒策略形状（只取界面要编辑的那几项）。 */
interface WakePolicy {
  provider?: string;
  at_mention?: boolean;
  reply?: boolean;
  name_mention?: boolean;
  names?: string[];
  accumulate?: number | null;
  keywords?: string[];
  poke?: boolean;
}

/** 该会话每个订阅的唤醒策略：房间串 → 策略。 */
const wakes = computed<Record<string, WakePolicy>>(() => {
  const out: Record<string, WakePolicy> = {};
  const session = (config.data.value?.sessions ?? []).find((item) => item.id === props.sessionId);
  for (const sub of session?.subscriptions ?? []) {
    if (typeof sub === "string") continue;
    const record = sub as Record<string, unknown>;
    const room = typeof record.room === "string" ? record.room : "";
    if (!room) continue;
    out[room] = (record.wake ?? {}) as WakePolicy;
  }
  return out;
});

/**
 * 一条策略用一句话说清楚「什么情况下会叫醒它」。
 *
 * 订阅列表上原来只有一串裸 OpenID——用户看不出「我 @ 它会不会理」。
 */
function wakeSummary(room: string): string {
  const wake = wakes.value[room];
  if (!wake) return t("console.rooms.wakeInherit");
  const parts: string[] = [];
  if (wake.at_mention) parts.push(t("console.rooms.wakeAt"));
  if (wake.reply) parts.push(t("console.rooms.wakeReply"));
  if (wake.name_mention) {
    const names = (wake.names ?? []).filter(Boolean);
    parts.push(
      names.length
        ? t("console.rooms.wakeName", { names: names.join("/") })
        : t("console.rooms.wakeNameNoList"),
    );
  }
  if (wake.accumulate && wake.accumulate > 0) {
    parts.push(t("console.rooms.wakeAccumulate", { count: wake.accumulate }));
  }
  return parts.length ? parts.join(" · ") : t("console.rooms.wakeNever");
}

/** 正在编辑唤醒策略的房间；`null` = 对话框关着。 */
const wakeRoom = ref<string | null>(null);
const wakeDraft = ref<{
  at_mention: boolean;
  reply: boolean;
  name_mention: boolean;
  names: string;
  accumulate: string;
}>({ at_mention: true, reply: true, name_mention: false, names: "", accumulate: "" });

function openWake(room: string): void {
  const wake = wakes.value[room] ?? {};
  wakeDraft.value = {
    at_mention: wake.at_mention !== false,
    reply: wake.reply !== false,
    name_mention: wake.name_mention === true,
    names: (wake.names ?? []).join(", "),
    accumulate: wake.accumulate == null ? "" : String(wake.accumulate),
  };
  wakeRoom.value = room;
}

/** 保存唤醒策略：走同一条 `subscription` op（传唤醒字段即覆盖）。 */
const saveWake = useMutation({
  mutationFn: () =>
    api.post<{ ok?: boolean; error?: string; detail?: string; changes?: string[] }>(
      "/api/config/patch",
      {
        ops: [
          {
            op: "subscription",
            session: props.sessionId,
            room: wakeRoom.value,
            wake: {
              at_mention: wakeDraft.value.at_mention,
              reply: wakeDraft.value.reply,
              name_mention: wakeDraft.value.name_mention,
              names: wakeDraft.value.names
                .split(",")
                .map((name) => name.trim())
                .filter(Boolean),
              accumulate: wakeDraft.value.accumulate.trim() === ""
                ? null
                : Number(wakeDraft.value.accumulate),
            },
          },
        ],
      },
    ),
  onSuccess: async (reply) => {
    if (reply?.error) {
      notice.value = reply.detail ? `${reply.error}（${reply.detail}）` : reply.error;
      return;
    }
    notice.value = t("console.models.saved");
    wakeRoom.value = null;
    await client.invalidateQueries({ queryKey: queryKeys.config });
    await client.invalidateQueries({ queryKey: queryKeys.sessions });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

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
  if (blocked.value) {
    // 用户在候选外手输了一个从没出现过消息的房间。ADR-0035 的裁定是
    // 「没有这个群则报错，也不维护也不提交」，所以这里直接拦住。
    notice.value = t("console.rooms.cannotSubmit");
    return;
  }
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
    <!-- 订阅条目：房间串 + 一句话的唤醒策略。
         原来是「一串裸 OpenID + 一个 ✕」，用户看不出「我 @ 它会不会理」。 -->
    <v-list density="compact" class="mb-2 pa-0">
      <v-list-item v-for="room in subscriptions" :key="room" class="px-0">
        <template #prepend>
          <v-icon :icon="icons.info" size="small" class="me-2" />
        </template>
        <v-list-item-title class="text-body-2">
          <code>{{ room }}</code>
        </v-list-item-title>
        <v-list-item-subtitle class="text-caption">
          {{ wakeSummary(room) }}
        </v-list-item-subtitle>
        <template #append>
          <v-btn
            icon
            size="x-small"
            variant="text"
            :aria-label="t('console.rooms.editWake')"
            @click="openWake(room)"
          >
            <v-icon :icon="icons.swap" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="error"
            :aria-label="t('console.rooms.remove')"
            :disabled="save.isPending.value"
            @click="remove(room)"
          >
            <v-icon :icon="icons.remove" size="small" />
          </v-btn>
        </template>
      </v-list-item>
    </v-list>

    <v-select
      :model-value="connection"
      :items="connections"
      item-title="title"
      item-value="value"
      :label="t('console.rooms.connection')"
      density="comfortable"
      variant="outlined"
      hide-details
      class="mt-3"
      style="max-width: 18rem"
      @update:model-value="onConnectionChange($event as string)"
    />

    <div class="d-flex ga-2 mt-2 flex-wrap align-start">
      <div style="flex: 1 1 22rem; min-width: 0">
        <RoomPicker
          ref="picker"
          v-model="draftRoom"
          :connection="connection"
          :label="t('console.rooms.add')"
        />
      </div>
      <v-btn
        color="primary"
        :disabled="save.isPending.value || !draftRoom.trim() || blocked"
        @click="add"
      >
        {{ t("console.rooms.add") }}
      </v-btn>
    </div>

    <div v-if="notice" class="text-caption text-medium-emphasis mt-2">{{ notice }}</div>

    <!-- 唤醒策略编辑：决定「什么情况下才叫醒 agent」。 -->
    <v-dialog
      :model-value="wakeRoom !== null"
      max-width="32rem"
      @update:model-value="wakeRoom = null"
    >
      <v-card>
        <v-card-title>{{ t("console.rooms.wakeTitle", { room: wakeRoom ?? "" }) }}</v-card-title>
        <v-card-text>
          <p class="text-caption text-medium-emphasis mb-3">
            {{ t("console.rooms.wakeHint") }}
          </p>
          <v-switch
            v-model="wakeDraft.at_mention"
            :label="t('console.rooms.wakeAt')"
            color="primary"
            density="compact"
            inset
            hide-details
          />
          <v-switch
            v-model="wakeDraft.reply"
            :label="t('console.rooms.wakeReply')"
            color="primary"
            density="compact"
            inset
            hide-details
          />
          <v-switch
            v-model="wakeDraft.name_mention"
            :label="t('console.rooms.wakeNameLabel')"
            color="primary"
            density="compact"
            inset
            hide-details
          />
          <v-text-field
            v-model="wakeDraft.names"
            :label="t('console.rooms.wakeNames')"
            :hint="t('console.rooms.wakeNamesHint')"
            persistent-hint
            density="compact"
            variant="outlined"
            class="mt-2"
            :disabled="!wakeDraft.name_mention"
          />
          <v-text-field
            v-model="wakeDraft.accumulate"
            type="number"
            min="0"
            :label="t('console.rooms.wakeAccumulateLabel')"
            :hint="t('console.rooms.wakeAccumulateHint')"
            persistent-hint
            density="compact"
            variant="outlined"
            class="mt-4"
          />
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="wakeRoom = null">{{ t("console.feedback.cancel") }}</v-btn>
          <v-btn
            color="primary"
            :loading="saveWake.isPending.value"
            @click="saveWake.mutate()"
          >
            {{ t("console.policy.save") }}
          </v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
  </div>
</template>
