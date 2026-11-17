<script setup lang="ts">
/**
 * 权限页。
 *
 * 页面上只有四块结构（规则 / 房间 / 管理员 / 角色），每块一个 `v-data-table`，
 * 解释全部进 `PageCard` 的 ⓘ。上一版手写的 pill、section 标题、表格样式全部删掉。
 */

import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { PolicyView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";

const { t } = useI18n();

const policy = useQuery({
  queryKey: queryKeys.policy,
  queryFn: () => api.get<PolicyView>("/api/policy"),
});

// 后端把策略放在 `grants` 里，不摊平在顶层（见 types.ts 的 PolicyView）。
const grants = computed(() => policy.data.value?.grants ?? {});
const rules = computed(() => grants.value.rules ?? []);
interface RoomRow {
  room: string;
  trust?: string;
  approval_route?: string;
}

const rooms = computed<RoomRow[]>(() =>
  Object.entries(grants.value.rooms ?? {}).map(([room, config]) => ({
    room,
    ...(config as Omit<RoomRow, "room">),
  })),
);
/**
 * 管理员：后端是 `system_admin` + `bot_admin` 两个字段，没有 `admins` 数组。
 * 拼成统一的行给表格用。
 */
const admins = computed(() => {
  const rows: Array<{ kind: string; account: string; connection: string }> = [];
  for (const [kind, value] of [
    ["system_admin", grants.value.system_admin],
    ["bot_admin", grants.value.bot_admin],
  ] as const) {
    if (!value) continue;
    const record = value as Record<string, unknown>;
    rows.push({
      kind,
      account: String(record.account ?? ""),
      connection: String(record.connection ?? ""),
    });
  }
  return rows;
});
const roles = computed(() => grants.value.roles ?? []);

const ruleHeaders = computed(() => [
  { title: t("console.policy.col.effect"), key: "effect" },
  { title: t("console.policy.col.subject"), key: "subject" },
  { title: t("console.policy.col.capabilities"), key: "capabilities" },
  { title: t("console.policy.col.rooms"), key: "rooms" },
]);

const roomHeaders = computed(() => [
  { title: t("console.policy.col.room"), key: "room" },
  { title: t("console.policy.col.trust"), key: "trust" },
  { title: t("console.policy.col.route"), key: "approval_route" },
]);

const roleHeaders = computed(() => [
  { title: "id", key: "id" },
  { title: t("console.policy.col.capabilities"), key: "capabilities" },
  { title: t("console.policy.col.source"), key: "source" },
]);

function isAllow(rule: Record<string, unknown>): boolean {
  return rule.allow === true || String(rule.allow) === "true";
}

function joinList(value: unknown): string {
  return Array.isArray(value) ? value.join(", ") : t("console.deps.none");
}
</script>

<template>
  <PageCard title-key="console.policy.title" tip-key="console.policy.intro">
    <template #actions>
      <v-btn
        size="small"
        variant="text"
        prepend-icon="mdi-refresh"
        :loading="policy.isFetching.value"
        @click="policy.refetch()"
      >
        {{ t("console.deps.refresh") }}
      </v-btn>
    </template>

    <QueryState :query="policy">
      <h3 class="text-subtitle-2 mb-2">
        {{ t("console.policy.rules") }}
        <span class="text-medium-emphasis">（{{ t("console.policy.count", { count: rules.length }) }}）</span>
      </h3>
      <v-data-table
        :headers="ruleHeaders"
        :items="rules"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.policy.noRules')"
      >
        <template #item.effect="{ item }">
          <v-chip :color="isAllow(item) ? 'success' : 'error'" size="small" variant="tonal">
            {{ isAllow(item) ? t("console.policy.allow") : t("console.policy.deny") }}
          </v-chip>
        </template>
        <template #item.subject="{ item }">
          <code class="text-caption">{{ item.subject || item.account || item.member || t("console.deps.none") }}</code>
        </template>
        <template #item.capabilities="{ item }">
          <span class="text-caption text-medium-emphasis">{{ joinList(item.capabilities) }}</span>
        </template>
        <template #item.rooms="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ Array.isArray(item.rooms) && item.rooms.length ? joinList(item.rooms) : t("console.policy.allRooms") }}
          </span>
        </template>
      </v-data-table>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.rooms") }}</h3>
      <v-data-table
        :headers="roomHeaders"
        :items="rooms"
        item-value="room"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.policy.noRooms')"
      >
        <template #item.room="{ item }">
          <code class="text-caption">{{ item.room }}</code>
        </template>
        <template #item.trust="{ item }">
          <v-chip :color="item.trust === 'trusted' ? 'success' : 'default'" size="small" variant="tonal">
            {{ item.trust === "trusted" ? t("console.policy.trusted") : t("console.policy.untrusted") }}
          </v-chip>
        </template>
        <template #item.approval_route="{ item }">
          <span class="text-caption text-medium-emphasis">{{ item.approval_route || t("console.deps.none") }}</span>
        </template>
      </v-data-table>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.admins") }}</h3>
      <div v-if="!admins.length" class="text-body-2 text-medium-emphasis">
        {{ t("console.policy.noAdmins") }}
      </div>
      <div v-else class="d-flex flex-wrap ga-2">
        <v-chip
          v-for="admin in admins"
          :key="`${admin.kind}:${admin.connection}:${admin.account}`"
          size="small"
          variant="tonal"
        >
          <span class="text-medium-emphasis me-1">{{ admin.kind }}</span>
          <code>{{ admin.connection ? `${admin.connection}:${admin.account}` : admin.account }}</code>
        </v-chip>
      </div>

      <template v-if="roles.length">
        <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.roles") }}</h3>
        <v-data-table
          :headers="roleHeaders"
          :items="roles"
          item-value="id"
          density="comfortable"
          :items-per-page="20"
        >
          <template #item.capabilities="{ item }">
            <span class="text-caption text-medium-emphasis">{{ joinList(item.capabilities) }}</span>
          </template>
          <template #item.source="{ item }">
            <span class="text-caption text-medium-emphasis">{{ item.source || t("console.deps.none") }}</span>
          </template>
        </v-data-table>
      </template>
    </QueryState>
  </PageCard>
</template>
