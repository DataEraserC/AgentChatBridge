<script setup lang="ts">
/**
 * 模型页：后备链 + 失败惩罚 + harness 模型列表。
 *
 * 对应两条用户诉求：链按会话配置（范围下拉），失败惩罚可配（四个字段）。
 * 交互全部用 Vuetify：范围是 `v-select`、链是 `v-data-table` + 行内按钮、
 * 惩罚是 `v-switch` + `v-text-field type=number`、被罚模型是 `v-chip`。
 * 上一版手写的 48 行样式、`.notice`、`.section`、`.buttons` 全部删掉。
 */

import { computed } from "vue";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { api } from "../api";
import { useI18n } from "../i18n";
import { queryKeys } from "../queryKeys";
import type {
  ModelFallbacksSaved,
  ModelFallbacksView,
  PenaltyPolicy,
  PenaltyView,
  SessionsView,
} from "../types";
import PageCard from "../components/PageCard.vue";
import QueryState from "../components/QueryState.vue";
import { ref, watch } from "vue";
import { icons } from "../icons";

const { t } = useI18n();
const client = useQueryClient();

/** null = 全局默认；字符串 = 某个会话自己的链。 */
const scope = ref<string | null>(null);
const notice = ref("");
const policyNotice = ref("");

const sessions = useQuery({
  queryKey: queryKeys.sessions,
  queryFn: () => api.get<SessionsView>("/api/sessions"),
});

const chain = useQuery({
  queryKey: computed(() => queryKeys.modelFallbacks(scope.value)),
  queryFn: () =>
    api.get<ModelFallbacksView>(
      scope.value
        ? `/api/model-fallbacks?session=${encodeURIComponent(scope.value)}`
        : "/api/model-fallbacks",
    ),
});

/**
 * 全局默认惩罚策略。
 *
 * 作用域是**模型级**（用户裁定 2026-11-17）：全局这份是「没单独配的模型都用它」，
 * 每个模型可以在行内单独覆盖。所以这里不再按 `scope`（会话）查——那一层已经去掉。
 */
const penalty = useQuery({
  queryKey: queryKeys.modelPenalty(""),
  queryFn: () => api.get<PenaltyView>("/api/model-penalty"),
});

/** 某个模型自己的策略（对话框里查）；`null` = 没配，回落全局。 */
const modelPolicy = useQuery({
  queryKey: computed(() => queryKeys.modelPenalty(editingModel.value ?? "")),
  queryFn: () =>
    api.get<PenaltyView>(
      `/api/model-penalty?model=${encodeURIComponent(editingModel.value ?? "")}`,
    ),
  enabled: computed(() => !!editingModel.value),
});

/** 正在编辑策略的模型 id；`null` = 对话框关着。 */
const editingModel = ref<string | null>(null);
const modelDraft = ref<PenaltyPolicy>({
  enabled: false,
  failure_threshold: 0,
  failure_window_secs: 0,
  penalty_secs: 0,
});
const modelNotice = ref("");

watch(
  () => modelPolicy.data.value?.policy,
  (policy) => {
    if (policy) modelDraft.value = { ...policy };
  },
  { immediate: true },
);

/** 打开某个模型的策略对话框。 */
function openModelPolicy(model: string): void {
  editingModel.value = model;
  modelNotice.value = "";
}

const saveModelPolicy = useMutation({
  mutationFn: ({ model, policy }: { model: string; policy: PenaltyPolicy | null }) =>
    api.post("/api/model-penalty", { model, policy }),
  onSuccess: async () => {
    modelNotice.value = t("console.penalty.saved");
    editingModel.value = null;
    await client.invalidateQueries({ queryKey: ["model-penalty"] });
  },
  onError: (error) => {
    modelNotice.value = error instanceof Error ? error.message : String(error);
  },
});

/** 范围改了，两条查询都按新键重取（不用手工 refetch）。 */
watch(scope, () => {
  notice.value = "";
  policyNotice.value = "";
});

const scopeItems = computed(() => [
  { value: null, title: t("console.models.globalScope") },
  ...(sessions.data.value?.sessions ?? []).map((session) => ({
    value: session.id,
    title: `${session.id} (${session.harness})`,
  })),
]);

const models = computed(() => chain.data.value?.models ?? []);
const available = computed(() => chain.data.value?.available ?? []);

const chainHeaders = computed(() => [
  { title: "#", key: "index", width: 60 },
  { title: t("console.models.col.model"), key: "model" },
  { title: t("console.models.col.state"), key: "position" },
  { title: "", key: "actions", sortable: false },
]);

const chainRows = computed(() =>
  models.value.map((model, index) => ({ index: index + 1, model, position: index })),
);

const save = useMutation({
  mutationFn: (next: string[]) =>
    api.post<ModelFallbacksSaved>("/api/model-fallbacks", { session: scope.value, models: next }),
  onSuccess: async () => {
    notice.value = t("console.models.saved");
    await client.invalidateQueries({ queryKey: ["model-fallbacks"] });
  },
  onError: (error) => {
    // 后端给的是「哪一项不合法、缺哪个 harness 模型」，直接显示。
    notice.value = error instanceof Error ? error.message : String(error);
  },
});

const pendingModel = ref("");

function add(): void {
  const value = pendingModel.value.trim();
  if (!value) return;
  if (models.value.includes(value)) {
    notice.value = t("console.models.duplicate", { model: value });
    return;
  }
  save.mutate([...models.value, value], { onSuccess: () => (pendingModel.value = "") });
}

function removeAt(index: number): void {
  const next = [...models.value];
  next.splice(index, 1);
  save.mutate(next);
}

function move(index: number, delta: number): void {
  const next = [...models.value];
  const target = index + delta;
  if (target < 0 || target >= next.length) return;
  [next[index], next[target]] = [next[target], next[index]];
  save.mutate(next);
}

async function adoptGlobal(): Promise<void> {
  const global = await api.get<ModelFallbacksView>("/api/model-fallbacks");
  if (!global.models.length) {
    notice.value = t("console.models.globalEmpty");
    return;
  }
  save.mutate([...global.models]);
}

/** 惩罚策略草稿：库里的现状 → 表单，保存后回读覆盖。 */
const policyDraft = ref<PenaltyPolicy>({
  enabled: false,
  failure_threshold: 0,
  failure_window_secs: 0,
  penalty_secs: 0,
});

watch(
  () => penalty.data.value?.policy,
  (policy) => {
    if (policy) policyDraft.value = { ...policy };
  },
  { immediate: true },
);

const savePolicy = useMutation({
  mutationFn: (policy: PenaltyPolicy) =>
    // 不带 `model` = 改全局默认那一份。
    api.post("/api/model-penalty", { policy }),
  onSuccess: async () => {
    policyNotice.value = t("console.penalty.saved");
    await client.invalidateQueries({ queryKey: ["model-penalty"] });
  },
  onError: (error) => {
    policyNotice.value = error instanceof Error ? error.message : String(error);
  },
});

const penalties = computed(() => penalty.data.value?.penalties ?? []);
</script>

<template>
  <PageCard title-key="console.models.title" tip-key="console.models.intro">
    <template #actions>
      <v-btn
        size="small"
        variant="text"
        :prepend-icon="icons.refresh"
        :loading="chain.isFetching.value"
        @click="chain.refetch()"
      >
        {{ t("console.deps.refresh") }}
      </v-btn>
    </template>

    <v-select
      v-model="scope"
      :items="scopeItems"
      item-title="title"
      item-value="value"
      density="comfortable"
      :label="t('console.models.scope')"
      class="mb-3"
      style="max-width: 26rem"
    />

    <QueryState :query="chain">
      <v-alert
        v-if="chain.data.value?.source === 'memory'"
        type="warning"
        variant="tonal"
        density="compact"
        class="mb-3"
        :text="t('console.models.memorySource')"
      />
      <v-alert v-if="notice" type="info" variant="tonal" density="compact" class="mb-3" :text="notice" />

      <v-alert
        v-if="chain.data.value?.inherited"
        type="warning"
        variant="tonal"
        density="compact"
        class="mb-3 d-flex align-center"
      >
        <span class="me-3">{{ t("console.models.inherited") }}</span>
        <v-btn size="small" variant="flat" :disabled="save.isPending.value" @click="adoptGlobal">
          {{ t("console.models.adopt") }}
        </v-btn>
      </v-alert>

      <v-data-table
        :headers="chainHeaders"
        :items="chainRows"
        item-value="model"
        density="comfortable"
        :items-per-page="20"
        :no-data-text="t('console.models.empty')"
      >
        <template #item.model="{ item }">
          <code class="text-caption">{{ item.model }}</code>
        </template>
        <template #item.position="{ item }">
          <span class="text-caption text-medium-emphasis">
            {{ item.position === 0 ? t("console.models.primary") : t("console.models.fallback") }}
          </span>
        </template>
        <template #item.actions="{ item }">
          <!-- 每个模型单独的策略：失败几次罚、罚多久。默认继承下面那块「全局默认」。 -->
          <v-btn
            icon
            size="x-small"
            variant="text"
            :aria-label="t('console.penalty.perModel')"
            @click="openModelPolicy(item.model)"
          >
            <v-icon :icon="icons.key" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            :disabled="save.isPending.value || item.position === 0"
            :aria-label="t('console.models.up')"
            @click="move(item.position, -1)"
          >
            <v-icon :icon="icons.arrowUp" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            :disabled="save.isPending.value || item.position === models.length - 1"
            :aria-label="t('console.models.down')"
            @click="move(item.position, 1)"
          >
            <v-icon :icon="icons.arrowDown" size="small" />
          </v-btn>
          <v-btn
            icon
            size="x-small"
            variant="text"
            color="error"
            :disabled="save.isPending.value"
            :aria-label="t('console.models.remove')"
            @click="removeAt(item.position)"
          >
            <v-icon :icon="icons.remove" size="small" />
          </v-btn>
        </template>
      </v-data-table>

      <div class="d-flex ga-2 mt-3 flex-wrap">
        <v-combobox
          v-model="pendingModel"
          :items="available"
          :label="t('console.models.placeholder')"
          density="comfortable"
          hide-details
          style="max-width: 32rem; flex: 1 1 20rem"
          @keydown.enter.prevent="add"
        />
        <v-btn
          color="primary"
          :disabled="save.isPending.value || !pendingModel"
          @click="add"
        >
          {{ t("console.models.add") }}
        </v-btn>
      </div>
      <p class="text-caption text-medium-emphasis mt-2">
        {{
          scope
            ? t("console.models.availableHint", {
                count: available.length,
                harness: chain.data.value?.harness ?? "",
              })
            : t("console.models.availableGlobalHint")
        }}
      </p>
    </QueryState>
  </PageCard>

  <PageCard title-key="console.penalty.title" tip-key="console.penalty.intro">
    <QueryState :query="penalty">
      <v-alert
        v-if="penalty.data.value?.inherited"
        type="info"
        variant="tonal"
        density="compact"
        class="mb-3"
        :text="t('console.penalty.inherited')"
      />

      <v-row dense>
        <v-col cols="12" sm="6" md="3">
          <v-switch
            v-model="policyDraft.enabled"
            :label="t('console.penalty.enabled')"
            color="primary"
            inset
          />
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-text-field
            v-model.number="policyDraft.failure_threshold"
            type="number"
            min="0"
            :label="t('console.penalty.threshold')"
            :hint="t('console.penalty.thresholdHint')"
            persistent-hint
          />
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-text-field
            v-model.number="policyDraft.failure_window_secs"
            type="number"
            min="0"
            :label="t('console.penalty.window')"
          />
        </v-col>
        <v-col cols="12" sm="6" md="3">
          <v-text-field
            v-model.number="policyDraft.penalty_secs"
            type="number"
            min="0"
            :label="t('console.penalty.duration')"
          />
        </v-col>
      </v-row>

      <v-alert
        v-if="policyNotice"
        type="info"
        variant="tonal"
        density="compact"
        class="mb-3"
        :text="policyNotice"
      />

      <div class="d-flex justify-end">
        <v-btn
          color="primary"
          size="small"
          :loading="savePolicy.isPending.value"
          :disabled="savePolicy.isPending.value"
          @click="savePolicy.mutate({ ...policyDraft })"
        >
          {{ t("console.penalty.save") }}
        </v-btn>
      </div>

      <h3 class="text-subtitle-2 mt-6 mb-2">{{ t("console.penalty.active") }}</h3>
      <div v-if="!penalties.length" class="text-body-2 text-medium-emphasis">
        {{ t("console.penalty.none") }}
      </div>
      <div v-else class="d-flex flex-wrap ga-2">
        <v-chip
          v-for="item in penalties"
          :key="item.model + item.class"
          color="warning"
          variant="tonal"
          size="small"
        >
          <code>{{ item.model }}</code>
          <span class="ms-2">
            {{ item.class }} · {{ t("console.penalty.seconds", { secs: item.remaining_secs }) }}
          </span>
        </v-chip>
      </div>
    </QueryState>

  <!-- 单个模型的策略：默认继承全局，可以在这里覆盖。 -->
  <v-dialog
    :model-value="editingModel !== null"
    max-width="34rem"
    @update:model-value="editingModel = null"
  >
    <v-card>
      <v-card-title>{{ t("console.penalty.perModelTitle", { model: editingModel ?? "" }) }}</v-card-title>
      <v-card-text>
        <p class="text-caption text-medium-emphasis mb-3">
          {{ t("console.penalty.perModelHint") }}
        </p>
        <v-switch
          v-model="modelDraft.enabled"
          :label="t('console.penalty.enabled')"
          color="primary"
          inset
        />
        <v-row dense>
          <v-col cols="12" sm="4">
            <v-text-field
              v-model.number="modelDraft.failure_threshold"
              type="number"
              min="0"
              :label="t('console.penalty.threshold')"
              density="comfortable"
              variant="outlined"
            />
          </v-col>
          <v-col cols="12" sm="4">
            <v-text-field
              v-model.number="modelDraft.failure_window_secs"
              type="number"
              min="0"
              :label="t('console.penalty.window')"
              density="comfortable"
              variant="outlined"
            />
          </v-col>
          <v-col cols="12" sm="4">
            <v-text-field
              v-model.number="modelDraft.penalty_secs"
              type="number"
              min="0"
              :label="t('console.penalty.duration')"
              density="comfortable"
              variant="outlined"
            />
          </v-col>
        </v-row>
        <v-alert
          v-if="modelNotice"
          type="info"
          variant="tonal"
          density="compact"
          class="mt-2"
          :text="modelNotice"
        />
      </v-card-text>
      <v-card-actions>
        <v-btn
          variant="text"
          color="error"
          :disabled="saveModelPolicy.isPending.value"
          @click="saveModelPolicy.mutate({ model: editingModel!, policy: null })"
        >
          {{ t("console.penalty.inherit") }}
        </v-btn>
        <v-spacer />
        <v-btn variant="text" @click="editingModel = null">{{ t("console.feedback.cancel") }}</v-btn>
        <v-btn
          color="primary"
          :loading="saveModelPolicy.isPending.value"
          @click="saveModelPolicy.mutate({ model: editingModel!, policy: modelDraft })"
        >
          {{ t("console.policy.save") }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
  </PageCard>
</template>
