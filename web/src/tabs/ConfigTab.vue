<script setup lang="ts">
/**
 * 配置页：当前生效配置（只读）+ TOML 原文（可编辑校验）。
 *
 * 表格用 `v-data-table`，JSON 值用 `v-chip`/文本呈现，TOML 用
 * `v-textarea`（自带滚动、等宽可配）。保存/校验/错误态走同一套 v-alert。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import { icons } from "../icons";

const { t } = useI18n();
const client = useQueryClient();

const config = useQuery({
  queryKey: queryKeys.config,
  queryFn: () => api.get<Record<string, unknown>>("/api/config"),
});

interface TomlView {
  path?: string;
  text: string;
  staged?: boolean;
}

const toml = useQuery({
  queryKey: queryKeys.configToml,
  queryFn: () => api.get<TomlView>("/api/config/toml"),
  // 原文含明文密钥，不自动重取；只在用户点「打开/重新读取」时取。
  enabled: false,
  retry: false,
});

const draft = ref("");
const notice = ref("");

const saveToml = useMutation({
  mutationFn: (text: string) =>
    api.post<{ ok?: boolean; error?: string; detail?: string }>("/api/config/toml", { text }),
  onSuccess: async (reply) => {
    if (reply?.error) {
      notice.value = reply.detail ? `${reply.error}（${reply.detail}）` : reply.error;
      return;
    }
    notice.value = t("console.config.saved");
    await Promise.all([
      client.invalidateQueries({ queryKey: queryKeys.config }),
      client.invalidateQueries({ queryKey: queryKeys.configToml }),
    ]);
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

async function openToml(): Promise<void> {
  await toml.refetch();
  draft.value = toml.data.value?.text ?? "";
}

/**
 * 扁平视图分两组：业务对象（数组，看条数）与启动参数（标量/小对象，看值）。
 * 只挑非空数组会让刚起的桥显示"暂无数据"，而它明明有生效中的标量。
 */
const groups = computed(() => {
  const data = config.data.value ?? {};
  const collections: Array<[string, unknown[]]> = [];
  const scalars: Array<[string, unknown]> = [];
  for (const [key, value] of Object.entries(data)) {
    if (Array.isArray(value)) collections.push([key, value]);
    else scalars.push([key, value]);
  }
  collections.sort((a, b) => a[0].localeCompare(b[0]));
  scalars.sort((a, b) => a[0].localeCompare(b[0]));
  return { collections, scalars };
});

function preview(value: unknown): string {
  if (Array.isArray(value)) return t("console.config.items", { count: value.length });
  if (value === null || value === undefined) return t("console.deps.none");
  if (typeof value === "object") {
    const pairs = Object.entries(value as Record<string, unknown>).map(
      ([k, v]) => `${k}=${v === null ? "null" : String(v)}`,
    );
    return pairs.length ? pairs.join("，") : t("console.config.items", { count: 0 });
  }
  return String(value);
}

const collectionHeaders = computed(() => [
  { title: t("console.config.col.section"), key: "name" },
  { title: t("console.config.col.count"), key: "count" },
]);

const scalarHeaders = computed(() => [
  { title: t("console.config.col.key"), key: "name" },
  { title: t("console.config.col.value"), key: "value" },
]);
</script>

<template>
  <PageCard title-key="console.config.title" tip-key="console.config.intro">
    <template #actions>
      <v-btn
        size="small"
        variant="text"
        :prepend-icon="icons.refresh"
        :loading="config.isFetching.value"
        @click="config.refetch()"
      >
        {{ t("console.config.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="config">
      <v-data-table
        :headers="collectionHeaders"
        :items="groups.collections.map(([name, value]) => ({ name, count: preview(value) }))"
        item-value="name"
        density="comfortable"
        :items-per-page="20"
      >
        <template #item.name="{ item }">
          <code class="text-caption">{{ item.name }}</code>
        </template>
        <template #item.count="{ item }">
          <span class="text-caption text-medium-emphasis">{{ item.count }}</span>
        </template>
      </v-data-table>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.config.scalars") }}</h3>
      <v-data-table
        :headers="scalarHeaders"
        :items="groups.scalars.map(([name, value]) => ({ name, value: preview(value) }))"
        item-value="name"
        density="comfortable"
        :items-per-page="30"
      >
        <template #item.name="{ item }">
          <code class="text-caption">{{ item.name }}</code>
        </template>
        <template #item.value="{ item }">
          <span class="text-caption text-medium-emphasis break-all">{{ item.value }}</span>
        </template>
      </v-data-table>
    </QueryState>
  </PageCard>

  <PageCard title-key="console.config.tomlTitle" tip-key="console.config.tomlIntro">
    <template #actions>
      <v-btn size="small" variant="text" :prepend-icon="icons.view" @click="openToml">
        {{ toml.data.value ? t("console.config.reload") : t("console.config.open") }}
      </v-btn>
    </template>

    <v-alert v-if="toml.isError.value" type="error" variant="tonal" density="comfortable">
      {{ toml.error.value instanceof Error ? toml.error.value.message : String(toml.error.value) }}
    </v-alert>

    <div v-else-if="!toml.data.value" class="text-body-2 text-medium-emphasis">
      {{ t("console.config.tomlClosed") }}
    </div>

    <template v-else>
      <p v-if="toml.data.value.path" class="text-caption text-medium-emphasis mb-2">
        <code>{{ toml.data.value.path }}</code>
      </p>
      <v-textarea
        v-model="draft"
        rows="18"
        spellcheck="false"
        variant="outlined"
        class="text-mono"
        :aria-label="t('console.config.tomlTitle')"
      />
      <v-alert v-if="notice" type="info" variant="tonal" density="compact" class="mt-2" :text="notice" />
      <div class="d-flex justify-end mt-2">
        <v-btn
          color="primary"
          size="small"
          :loading="saveToml.isPending.value"
          :disabled="saveToml.isPending.value"
          @click="saveToml.mutate(draft)"
        >
          {{ t("console.config.save") }}
        </v-btn>
      </div>
    </template>
  </PageCard>
</template>

<style scoped>
/* 只保留一件事：TOML 需要等宽字体。这个 Vuetify 不管，属于业务约定。 */
.text-mono :deep(textarea) {
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-size: 12px;
  line-height: 1.6;
}
</style>
