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
import { icons } from "../icons";

const { t } = useI18n();

const policy = useQuery({
  queryKey: queryKeys.policy,
  queryFn: () => api.get<PolicyView>("/api/policy"),
});

// 后端把策略放在 `grants` 里，不摊平在顶层（见 types.ts 的 PolicyView）。
const grants = computed(() => policy.data.value?.grants ?? {});
/**
 * 全部规则行 = 允许规则 + 拒绝规则，各带 `effect` 标记。
 *
 * 分开算再合并，而不是信任行内字段——效果由来源数组决定（见 `effectLabel`）。
 */
const rules = computed<Array<Record<string, unknown> & { effect: "allow" | "deny" }>>(() => [
  ...(grants.value.rules ?? []).map((rule) => ({ ...rule, effect: "allow" as const })),
  ...(grants.value.deny ?? []).map((rule) => ({ ...rule, effect: "deny" as const })),
]);
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

/**
 * 规则的列。
 *
 * **必须带上 `id` / `place` / `scope` / `target`**：只给「效果/主体/能力/房间」
 * 四列时，13 条规则在屏幕上长得一模一样（都是「拒绝 / system_admin / — / 全部
 * 房间」），看起来像重复数据。实际上它们靠 `id`（`group-769843907-issue` 之类）、
 * `place`（group/private）、`scope`（connection/room/subscribed）、`target`
 * 区分。2026-11-17 用户就是被这个误导的。
 */
/** 规则的 target → 一句人话。`null` = 不限定（全部）。 */
function describeTarget(target: unknown): string {
  if (!target || typeof target !== "object") return t("console.policy.allRooms");
  const record = target as Record<string, unknown>;
  const kind = String(record.kind ?? "");
  const id = String(record.id ?? "");
  const sub = record.sub == null ? "" : String(record.sub);
  if (!id) return t("console.policy.allRooms");
  return sub ? `${kind}:${id}/${sub}` : `${kind}:${id}`;
}

const ruleHeaders = computed(() => [
  { title: t("console.policy.col.id"), key: "id" },
  { title: t("console.policy.col.effect"), key: "effect" },
  { title: t("console.policy.col.subject"), key: "subject" },
  { title: t("console.policy.col.capabilities"), key: "capabilities" },
  { title: t("console.policy.col.place"), key: "place" },
  { title: t("console.policy.col.scope"), key: "scope" },
  { title: t("console.policy.col.target"), key: "target" },
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

/**
 * 规则是「允许」还是「拒绝」。
 *
 * **不能看 `rule.allow`**：`GrantRule` 结构里根本没有这个字段。后端把授权
 * 分成两个数组：
 *   - `rules` → 允许（"只做加法"）
 *   - `deny`  → 显式拒绝（"只做减法"）
 * 所以「来自哪个数组」就是效果本身，行里不用再带标记。
 *
 * 2026-11-17 修：前端原先判断 `rule.allow === true`，字段不存在 → 永远 false
 * → 13 条**允许**规则全被渲染成「拒绝」，看起来像「全拒，谁都不能用」。
 */
function effectLabel(isDeny: boolean): string {
  return isDeny ? t("console.policy.deny") : t("console.policy.allow");
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
        :prepend-icon="icons.refresh"
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
          <v-chip
            :color="item.effect === 'allow' ? 'success' : 'error'"
            size="small"
            variant="tonal"
          >
            {{ effectLabel(item.effect === 'deny') }}
          </v-chip>
        </template>
        <template #item.subject="{ item }">
          <code class="text-caption">{{ item.subject || item.account || item.member || t("console.deps.none") }}</code>
        </template>
        <template #item.id="{ item }">
          <code class="text-caption">{{ item.id }}</code>
        </template>
        <template #item.capabilities="{ item }">
          <!-- 后端字段是 `capability`（单数，数组），不是 `capabilities`。 -->
          <span class="text-caption text-medium-emphasis">{{ joinList(item.capability) }}</span>
        </template>
        <template #item.place="{ item }">
          <span class="text-caption">{{ item.place || "—" }}</span>
        </template>
        <template #item.scope="{ item }">
          <span class="text-caption">{{ item.scope || "—" }}</span>
        </template>
        <template #item.target="{ item }">
          <span class="text-caption">{{ describeTarget(item.target) }}</span>
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
