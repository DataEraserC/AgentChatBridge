<script setup lang="ts">
/**
 * 房间选择器。
 *
 * 目标（用户诉求）：**不再从日志里翻房间号**——能枚举就用下拉，不能就留手输。
 *
 * 三条设计约束：
 * 1. `supported = false`（适配器不实现枚举）→ 只给手输框，并说明原因。
 *    不能显示一个空下拉说"没有房间"，那是两种不同的空。
 * 2. `supported = true` 但列表为空 → 手输框 + 一句「这个号确实没有可选房间」。
 * 3. 永远接受手输：枚举可能被平台限流、可能只覆盖部分房间，堵死手输会把人
 *    逼回翻日志。
 *
 * 枚举是按连接问的：换连接就要重取，所以 connection 变化触发 refetch。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { RoomVerifyView, RoomsView } from "../types";

const props = defineProps<{
  /** 连接名。空 = 还不知道要问哪条连接，此时只给手输框。 */
  connection: string;
  /** 当前值（房间字符串，如 `qq:group/123`）。 */
  modelValue: string;
  label?: string;
}>();

const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const { t } = useI18n();

const rooms = useQuery({
  queryKey: computed(() => queryKeys.rooms(props.connection)),
  queryFn: () => api.get<RoomsView>(`/api/rooms?connection=${encodeURIComponent(props.connection)}`),
  // 没连接名就没法问
  enabled: computed(() => props.connection.trim().length > 0),
  // 枚举要打平台 API，别因为窗口聚焦就重新打一遍；TTL 在服务端。
  refetchOnWindowFocus: false,
  retry: false,
});

const supported = computed(() => rooms.data.value?.supported ?? false);

/**
 * 候选 = 适配器自报的 ∪ 来过消息的（ADR-0035）。
 *
 * 两个来源合并才有用：`rooms` 能给出「还没来过消息的房间」，`seen` 能给出
 * 「平台不给枚举、但确实在用的房间」。同一个房间同时出现时以 `rooms` 为准
 * （它有 label）。
 */
const items = computed(() => {
  const out: Array<{ value: string; title: string }> = [];
  const seenValues = new Set<string>();
  for (const choice of rooms.data.value?.rooms ?? []) {
    out.push({
      value: choice.room,
      title: choice.label ? `${choice.label}（${choice.room}）` : choice.room,
    });
    seenValues.add(choice.room);
  }
  for (const stat of rooms.data.value?.seen ?? []) {
    if (seenValues.has(stat.room)) continue;
    // 没 label（qqbot-official 拿不到群名）：用消息数当线索，比光秃秃的 id 有用。
    out.push({
      value: stat.room,
      title: `${stat.room}（${t("console.rooms.seenCount", { count: stat.messages })}）`,
    });
  }
  return out;
});

/** 提示：问不了 / 不支持且没见过的 / 完全没有候选。只说一句，解释在别处。 */
const note = computed(() => {
  if (!props.connection.trim()) return t("console.rooms.noConnection");
  if (rooms.isPending.value) return t("console.ui.state.loading");
  if (rooms.isError.value) return t("console.rooms.failed");
  if (items.value.length) return "";
  // 有 seen 就算有候选，走不到这里；真没有才是「确实空」。
  if (!supported.value) return t("console.rooms.unsupported");
  return t("console.rooms.empty");
});

/** 手输的确认状态。只有选了候选之外的值才需要校验。 */
const verify = ref<{ state: "idle" | "checking" | "known" | "unknown"; detail: string }>({
  state: "idle",
  detail: "",
});

const VERIFY = useMutation({
  mutationFn: (room: string) =>
    api.post<RoomVerifyView>("/api/rooms/verify", { room }),
});

/**
 * 用户敲完一个**不在候选里**的房间时校验一次（ADR-0035 的「确认一次」）。
 *
 * 只校验不提交：`known: false` 时由父组件阻止提交，这里不碰任何状态。
 * 在候选里的直接放过——已经在 `seen` 或 `rooms` 里，没必要再查一遍。
 */
async function checkManual(value: string): Promise<void> {
  const room = value.trim();
  if (!room) {
    verify.value = { state: "idle", detail: "" };
    return;
  }
  if (items.value.some((item) => item.value === room)) {
    verify.value = { state: "known", detail: "" };
    return;
  }
  verify.value = { state: "checking", detail: "" };
  try {
    const reply = await VERIFY.mutateAsync(room);
    verify.value = reply.known
      ? {
          state: "known",
          detail: t("console.rooms.verified", { count: reply.messages ?? 0 }),
        }
      : { state: "unknown", detail: t("console.rooms.unknown") };
  } catch (error) {
    verify.value = {
      state: "unknown",
      detail: error instanceof Error ? error.message : String(error),
    };
  }
}

/** 值变了：先告诉父组件，再看要不要校验。 */
function onInput(value: string | null): void {
  const next = value ?? "";
  emit("update:modelValue", next);
  void checkManual(next);
}

defineExpose({ verifyState: verify });
</script>

<template>
  <div>
    <!-- 有列表就带补全；没有就是纯手输 -->
    <v-combobox
      :model-value="modelValue"
      :items="items"
      item-title="title"
      item-value="value"
      :label="label ?? t('console.rooms.label')"
      :loading="rooms.isFetching.value"
      density="comfortable"
      variant="outlined"
      hide-details
      clearable
      @update:model-value="onInput($event as string)"
    >
      <template #append-inner>
        <v-btn
          icon
          size="x-small"
          variant="text"
          :loading="rooms.isFetching.value"
          :aria-label="t('console.rooms.refresh')"
          @click.stop="rooms.refetch()"
        >
          <v-icon icon="mdi-refresh" size="small" />
        </v-btn>
      </template>
    </v-combobox>
    <div v-if="note" class="text-caption text-medium-emphasis mt-1">{{ note }}</div>
    <!-- 手输校验结果：known 绿、unknown 红。只有真的敲了候选外的值才出现。 -->
    <div
      v-if="verify.state === 'checking'"
      class="text-caption text-medium-emphasis mt-1"
    >
      {{ t("console.rooms.checking") }}
    </div>
    <div
      v-else-if="verify.state === 'known' && verify.detail"
      class="text-caption text-success mt-1"
    >
      {{ verify.detail }}
    </div>
    <div v-else-if="verify.state === 'unknown'" class="text-caption text-error mt-1">
      {{ verify.detail }}
    </div>
  </div>
</template>
