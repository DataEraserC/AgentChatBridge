<script setup lang="ts">
/**
 * 插件页。
 *
 * 插件清单用 `v-data-table` 的 expand 槽展开配置表单——展开、箭头、键盘
 * 都由组件库处理，不需要自己维护「当前展开哪一行」。表单本身走 JSONForms，
 * 见 `PluginConfigForm`。
 */

import { computed, ref } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { PluginsView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import PluginConfigForm from "../components/PluginConfigForm.vue";

const { t } = useI18n();
const expanded = ref<string[]>([]);

const plugins = useQuery({
  queryKey: queryKeys.plugins,
  queryFn: () => api.get<PluginsView>("/api/plugins"),
});

const rows = computed(() => plugins.data.value?.plugins ?? []);

const headers = computed(() => [
  { title: t("console.plugins.col.id"), key: "id" },
  { title: t("console.plugins.col.extension"), key: "schema" },
  { title: t("console.plugins.col.state"), key: "enabled" },
  { title: t("console.plugins.col.health"), key: "connections" },
  { title: "", key: "data-table-expand" },
]);

/** 只有声明了配置字段的插件才可展开，避免展开一个空面板。 */
function fieldCount(plugin: { schema?: { config?: unknown[] } }): number {
  return plugin.schema?.config?.length ?? 0;
}
</script>

<template>
  <PageCard title-key="console.plugins.title" tip-key="console.plugins.intro">
    <template #actions>
      <v-btn
        size="small"
        variant="text"
        prepend-icon="mdi-refresh"
        :loading="plugins.isFetching.value"
        @click="plugins.refetch()"
      >
        {{ t("console.plugins.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="plugins" :empty="!rows.length" empty-key="console.plugins.empty">
      <v-data-table
        v-model:expanded="expanded"
        :headers="headers"
        :items="rows"
        item-value="id"
        density="comfortable"
        :items-per-page="20"
        show-expand
        :no-data-text="t('console.plugins.empty')"
      >
        <template #item.id="{ item }">
          <code class="text-caption">{{ item.id }}</code>
          <span v-if="item.version" class="text-caption text-medium-emphasis"> · {{ item.version }}</span>
        </template>
        <template #item.schema="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ item.schema ? t("console.plugins.hasSchema") : t("console.plugins.noSchema") }}
          </span>
        </template>
        <template #item.enabled="{ item }">
          <v-chip :color="item.enabled ? 'success' : 'default'" size="small" variant="tonal">
            {{ item.enabled ? t("console.plugins.enabled") : t("console.plugins.disabled") }}
          </v-chip>
        </template>
        <template #item.connections="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ item.connections?.length ? item.connections.length : t("console.deps.none") }}
          </span>
        </template>
        <!-- 展开行：没有配置字段的插件给一句说明，而不是空面板。 -->
        <template #expanded-row="{ columns, item }">
          <tr>
            <td :colspan="columns.length" class="pa-4">
              <PluginConfigForm
                v-if="fieldCount(item)"
                :plugin-id="item.id"
                :fields="item.schema?.config ?? []"
                :values="item.config ?? {}"
                @saved="plugins.refetch()"
              />
              <span v-else class="text-caption text-medium-emphasis">
                {{ t("console.plugins.noSchema") }}
              </span>
            </td>
          </tr>
        </template>
      </v-data-table>
    </QueryState>
  </PageCard>
</template>
