<script setup lang="ts">
/**
 * 权限页。
 *
 * 页面上只有四块结构（规则 / 房间 / 管理员 / 角色），每块一个 `v-data-table`，
 * 解释全部进 `PageCard` 的 ⓘ。上一版手写的 pill、section 标题、表格样式全部删掉。
 */

import { computed, ref } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type { PolicyView } from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import { icons } from "../icons";

const { t } = useI18n();
const client = useQueryClient();

const policy = useQuery({
  queryKey: queryKeys.policy,
  queryFn: () => api.get<PolicyView>("/api/policy"),
});

// 后端把策略放在 `grants` 里，不摊平在顶层（见 types.ts 的 PolicyView）。
const grants = computed(() => policy.data.value?.grants ?? {});

/**
 * 编辑草稿。
 *
 * 为什么用本地草稿而不是「每次改动立刻提交」：后端 `POST /api/policy` 收的是
 * **整份 grants**（它按事务一次落地，避免「规则改了一半」的中间态）。所以页面
 * 上也该是「改完一次提交」，否则每敲一个字都覆盖全表。
 *
 * `draft === null` 表示「没有未保存的改动」，此时显示库里的那份。
 */
type Draft = {
  rules: Array<Record<string, unknown>>;
  deny: Array<Record<string, unknown>>;
  /** 房间级政策：房间 id → { trust, members, approval_route }。 */
  rooms: Record<string, Record<string, unknown>>;
  system_admin: Record<string, unknown> | null;
  bot_admin: Record<string, unknown> | null;
  roles: Array<Record<string, unknown>>;
};
const draft = ref<Draft | null>(null);

/** 当前显示的行：有草稿用草稿，否则用库里的。 */
const currentRules = computed<Array<Record<string, unknown>>>(() => {
  if (draft.value) return draft.value.rules;
  return (grants.value.rules ?? []) as Array<Record<string, unknown>>;
});
const currentDeny = computed<Array<Record<string, unknown>>>(() => {
  if (draft.value) return draft.value.deny;
  return (grants.value.deny ?? []) as Array<Record<string, unknown>>;
});

/** 进入编辑态：把库里的规则拷一份到草稿。 */
function beginEdit(): void {
  const g = grants.value;
  // 深拷贝：草稿必须能在不碰原数据的前提下任意增删，否则取消编辑改不回去。
  draft.value = {
    rules: JSON.parse(JSON.stringify(currentRules.value)),
    deny: JSON.parse(JSON.stringify(currentDeny.value)),
    rooms: JSON.parse(JSON.stringify(g.rooms ?? {})),
    system_admin: JSON.parse(JSON.stringify(g.system_admin ?? null)),
    bot_admin: JSON.parse(JSON.stringify(g.bot_admin ?? null)),
    roles: JSON.parse(JSON.stringify(g.roles ?? [])),
  };
}

function cancelEdit(): void {
  draft.value = null;
  notice.value = "";
}

/** 删一条规则（按 id）。只改草稿，不提交。 */
function removeRule(effect: "allow" | "deny", id: string): void {
  if (!draft.value) beginEdit();
  if (!draft.value) return;
  const list = effect === "deny" ? draft.value.deny : draft.value.rules;
  const at = list.findIndex((rule) => String(rule.id) === id);
  if (at >= 0) list.splice(at, 1);
}

/** 加一条空白允许规则，让用户在表格里改。 */
function addRule(): void {
  if (!draft.value) beginEdit();
  if (!draft.value) return;
  draft.value.rules.push({
    id: `rule-${Date.now()}`,
    subject: "system_admin",
    place: "group",
    scope: "subscribed",
    capability: ["room_read"],
    permissions: [],
    target: null,
  });
}

const notice = ref("");

/** 保存：整份 grants 一次提交。 */
const save = useMutation({
  mutationFn: (payload: Record<string, unknown>) =>
    api.post<{
      ok?: boolean;
      error?: string;
      detail?: string;
      rules?: number;
      allow?: number;
      deny?: number;
      rooms?: number;
    }>(
      "/api/policy",
      payload,
    ),
  onSuccess: async (reply) => {
    if (reply?.error) {
      notice.value = reply.detail ? `${reply.error}（${reply.detail}）` : reply.error;
      return;
    }
    // 分开报：只说「N 条规则」而页面还有房间/管理员，用户会对不上。
    notice.value = t("console.policy.savedDetail", {
      allow: reply?.allow ?? 0,
      deny: reply?.deny ?? 0,
      rooms: reply?.rooms ?? 0,
    });
    draft.value = null;
    await client.invalidateQueries({ queryKey: queryKeys.policy });
  },
  onError: (error) => {
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

function submit(): void {
  if (!draft.value) return;
  const next = {
    ...grants.value,
    rules: draft.value.rules,
    deny: draft.value.deny,
    rooms: draft.value.rooms,
    system_admin: draft.value.system_admin,
    bot_admin: draft.value.bot_admin,
    roles: draft.value.roles,
  };
  save.mutate({ grants: next });
}
/**
 * 全部规则行 = 允许规则 + 拒绝规则，各带 `effect` 标记。
 *
 * 分开算再合并，而不是信任行内字段——效果由来源数组决定（见 `effectLabel`）。
 */
const rows = computed<Array<Record<string, unknown> & { effect: "allow" | "deny" }>>(() => [
  ...currentRules.value.map((rule) => ({ ...rule, effect: "allow" as const })),
  ...currentDeny.value.map((rule) => ({ ...rule, effect: "deny" as const })),
]);
/* ── 房间 / 管理员 / 角色的编辑（ADR：每个页面都要能增删改查） ────────── */

/** 当前显示的房间配置：编辑态用草稿，否则用库里的。 */
const currentRooms = computed<Record<string, Record<string, unknown>>>(() =>
  draft.value ? draft.value.rooms : ((grants.value.rooms ?? {}) as Record<string, Record<string, unknown>>),
);

/** 房间行：房间 id 摊平成表格要的形状。 */
const roomRows = computed(() =>
  Object.entries(currentRooms.value).map(([room, cfg]) => {
    const record = cfg as Record<string, unknown>;
    const route = record.approval_route;
    return {
      room,
      trust: String(record.trust ?? "untrusted"),
      approval_route: Array.isArray(route) ? (route as string[]) : [],
    };
  }),
);

/** 信任档位，与 `grants::Trust` 的 serde 表示一致。 */
const TRUST_OPTIONS = computed(() => [
  { value: "untrusted", title: t("console.policy.untrusted") },
  { value: "trusted", title: t("console.policy.trusted") },
]);

const currentSystemAdmin = computed<Record<string, unknown> | null>(() =>
  draft.value
    ? draft.value.system_admin
    : ((grants.value.system_admin ?? null) as Record<string, unknown> | null),
);

const currentRoles = computed<Array<Record<string, unknown>>>(() =>
  draft.value ? draft.value.roles : ((grants.value.roles ?? []) as Array<Record<string, unknown>>),
);

/** 改房间的信任档位。只动草稿，保存时才落库。 */
function setRoomTrust(room: string, trust: string): void {
  if (!draft.value) beginEdit();
  const rooms = draft.value?.rooms;
  if (!rooms) return;
  rooms[room] = { ...(rooms[room] ?? {}), trust };
}

function removeRoom(room: string): void {
  if (!draft.value) beginEdit();
  delete draft.value?.rooms[room];
}

/** 新增房间政策。房间 id 必须手输——后端没有「列出所有房间」的稳定来源
 *  （`seen` 只是有消息的房间，不等于可配范围）。 */
const newRoomId = ref("");
function addRoom(): void {
  const room = newRoomId.value.trim();
  if (!room) return;
  if (!draft.value) beginEdit();
  const rooms = draft.value?.rooms;
  if (!rooms) return;
  if (!rooms[room]) rooms[room] = { trust: "untrusted", members: [], approval_route: [] };
  newRoomId.value = "";
}

/** 改系统管理员的某一项。两项都空 = 没有管理员（后端用 null 表达）。 */
function setSystemAdmin(field: "connection" | "account", value: string): void {
  if (!draft.value) beginEdit();
  if (!draft.value) return;
  const next = { ...(draft.value.system_admin ?? {}), [field]: value.trim() };
  const empty = String(next.connection ?? "") === "" && String(next.account ?? "") === "";
  draft.value.system_admin = empty ? null : next;
}

function clearSystemAdmin(): void {
  if (!draft.value) beginEdit();
  if (draft.value) draft.value.system_admin = null;
}

/** 删角色。内置角色不给删：它们由迁移写入，删了下次启动会被补齐。 */
function removeRole(id: string): void {
  if (!draft.value) beginEdit();
  const roles = draft.value?.roles;
  if (!roles) return;
  const at = roles.findIndex((role) => String(role.id) === id);
  if (at >= 0 && roles[at].builtin !== true) roles.splice(at, 1);
}


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
  // 只有在编辑态才给操作列：平时是只读视图，多一列空按钮只会误导。
  ...(draft.value ? [{ title: "", key: "actions", sortable: false }] : []),
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
      <div class="d-flex align-center ga-2 mb-2">
        <h3 class="text-subtitle-2">
          {{ t("console.policy.rules") }}
          <span class="text-medium-emphasis">（{{ t("console.policy.count", { count: rows.length }) }}）</span>
        </h3>
        <v-spacer />
        <template v-if="!draft">
          <v-btn size="small" variant="text" :disabled="save.isPending.value" @click="beginEdit">
            {{ t("console.policy.edit") }}
          </v-btn>
          <v-btn size="small" variant="tonal" :disabled="save.isPending.value" @click="addRule">
            {{ t("console.policy.addRule") }}
          </v-btn>
        </template>
        <template v-else>
          <v-btn size="small" variant="text" :disabled="save.isPending.value" @click="cancelEdit">
            {{ t("console.policy.cancel") }}
          </v-btn>
          <v-btn
            size="small"
            color="primary"
            :loading="save.isPending.value"
            @click="submit"
          >
            {{ t("console.policy.save") }}
          </v-btn>
        </template>
      </div>
      <v-alert
        v-if="draft"
        type="warning"
        variant="tonal"
        density="compact"
        class="mb-3"
        :text="t('console.policy.unsaved')"
      />
      <v-alert
        v-if="notice"
        type="info"
        variant="tonal"
        density="compact"
        class="mb-3"
        :text="notice"
      />
      <v-data-table
        :headers="ruleHeaders"
        :items="rows"
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
        <template #item.actions="{ item }">
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="error"
            :aria-label="t('console.policy.remove')"
            @click="removeRule(item.effect as 'allow' | 'deny', String(item.id))"
          >
            <v-icon :icon="icons.remove" size="small" />
          </v-btn>
        </template>
      </v-data-table>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.rooms") }}</h3>
      <v-data-table
        :headers="roomHeaders"
        :items="roomRows"
        item-value="room"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.policy.noRooms')"
      >
        <template #item.room="{ item }">
          <code class="text-caption">{{ item.room }}</code>
        </template>
        <template #item.trust="{ item }">
          <v-select
            v-if="draft"
            :model-value="item.trust"
            :items="TRUST_OPTIONS"
            item-title="title"
            item-value="value"
            density="compact"
            variant="plain"
            hide-details
            style="max-width: 9rem"
            @update:model-value="setRoomTrust(item.room, String($event))"
          />
          <v-chip v-else :color="item.trust === 'trusted' ? 'success' : 'default'" size="small" variant="tonal">
            {{ item.trust === "trusted" ? t("console.policy.trusted") : t("console.policy.untrusted") }}
          </v-chip>
        </template>
        <template #item.approval_route="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ item.approval_route.length ? item.approval_route.join(", ") : t("console.deps.none") }}
          </span>
        </template>
        <template v-if="draft" #item.__actions="{ item }">
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="error"
            :aria-label="t('console.policy.remove')"
            @click="removeRoom(item.room)"
          >
            <v-icon :icon="icons.remove" size="small" />
          </v-btn>
        </template>
      </v-data-table>
      <div v-if="draft" class="d-flex ga-2 mt-2 align-center">
        <v-text-field
          v-model="newRoomId"
          :label="t('console.policy.addRoomLabel')"
          :hint="t('console.policy.addRoomHint')"
          persistent-hint
          density="compact"
          variant="outlined"
          hide-details
          style="max-width: 22rem"
        />
        <v-btn size="small" variant="tonal" :disabled="!newRoomId.trim()" @click="addRoom">
          {{ t("console.policy.addRoom") }}
        </v-btn>
      </div>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.admins") }}</h3>
      <!-- 编辑态：后端是单个 `system_admin` 对象，改它的 connection / account。 -->
      <div v-if="draft" class="d-flex ga-2 align-center flex-wrap">
        <v-text-field
          :model-value="String(currentSystemAdmin?.connection ?? '')"
          :label="t('console.rooms.connection')"
          density="compact"
          variant="outlined"
          hide-details
          style="max-width: 14rem"
          @update:model-value="setSystemAdmin('connection', String($event))"
        />
        <v-text-field
          :model-value="String(currentSystemAdmin?.account ?? '')"
          :label="t('console.policy.adminAccount')"
          density="compact"
          variant="outlined"
          hide-details
          style="max-width: 14rem"
          @update:model-value="setSystemAdmin('account', String($event))"
        />
        <v-btn size="small" variant="text" color="error" @click="clearSystemAdmin">
          {{ t("console.policy.clearAdmin") }}
        </v-btn>
      </div>
      <div v-else-if="!admins.length" class="text-body-2 text-medium-emphasis">
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

      <template v-if="currentRoles.length">
        <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.policy.roles") }}</h3>
        <v-data-table
          :headers="draft ? [...roleHeaders, { title: '', key: '__actions', sortable: false }] : roleHeaders"
          :items="currentRoles"
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
          <template v-if="draft" #item.__actions="{ item }">
            <v-btn
              v-if="item.builtin !== true"
              icon
              size="x-small"
              variant="text"
              color="error"
              :aria-label="t('console.policy.remove')"
              @click="removeRole(String(item.id))"
            >
              <v-icon :icon="icons.remove" size="small" />
            </v-btn>
            <span v-else class="text-caption text-medium-emphasis">
              {{ t("console.policy.builtin") }}
            </span>
          </template>
        </v-data-table>
      </template>
    </QueryState>
  </PageCard>
</template>
