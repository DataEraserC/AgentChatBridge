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

import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { RoomsView } from "../types";

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
const items = computed(() =>
  (rooms.data.value?.rooms ?? []).map((choice) => ({
    value: choice.room,
    title: choice.label ? `${choice.label}（${choice.room}）` : choice.room,
  })),
);

/** 三种提示：问不了 / 不支持 / 支持但为空。都只说一句，解释在别处。 */
const note = computed(() => {
  if (!props.connection.trim()) return t("console.rooms.noConnection");
  if (rooms.isPending.value) return t("console.ui.state.loading");
  if (rooms.isError.value) return t("console.rooms.failed");
  if (!supported.value) return t("console.rooms.unsupported");
  if (!items.value.length) return t("console.rooms.empty");
  return "";
});
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
      @update:model-value="emit('update:modelValue', ($event as string) ?? '')"
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
  </div>
</template>
