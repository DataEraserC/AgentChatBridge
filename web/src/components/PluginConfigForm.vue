<script setup lang="ts">
/**
 * 按插件自报的 schema 渲染配置表单 —— 用 JSONForms，不手写控件。
 *
 * 这是「插件自描述接口」在前端的落点，也是本次选 Vuetify 的核心原因：
 * `@jsonforms/vue-vuetify` 是官方渲染器，JSON Schema → Vuetify 控件（含 enum 下拉、
 * 范围校验、布尔开关、数组/对象）全部由库完成。我们只负责把插件声明翻译成
 * JSON Schema + UI Schema 这一对，以及把结果提交回 `/api/plugins/<id>/config`。
 *
 * 为什么不直接把 `SchemaField[]` 当 JSON Schema 用：两者形状接近但不等价（我们的
 * 字段是扁平数组，JSON Schema 要求 `properties` 对象），所以做一次显式翻译。
 * 这正是应该写的那一层薄代码——不是重造渲染器。
 */

import { computed, ref, watch } from "vue";
import { JsonForms } from "@jsonforms/vue";
import { vuetifyRenderers } from "@jsonforms/vue-vuetify";
import "@jsonforms/vue-vuetify/lib/jsonforms-vue-vuetify.css";
import { api } from "../api";
import { useI18n } from "../i18n";
import type { JsonSchema7 } from "@jsonforms/core";
import type { SchemaField } from "../types";

const props = defineProps<{
  pluginId: string;
  fields: SchemaField[];
  values: Record<string, unknown>;
}>();

const emit = defineEmits<{ saved: [] }>();

const { t } = useI18n();

const data = ref<Record<string, unknown>>({});
const saving = ref(false);
const notice = ref("");
const failed = ref(false);

function seed(): void {
  const next: Record<string, unknown> = {};
  for (const field of props.fields) {
    const current = props.values[field.key];
    if (current !== undefined) next[field.key] = current;
    else if (field.default !== undefined) next[field.key] = field.default;
  }
  data.value = next;
}

seed();
watch(() => props.values, seed, { deep: true });

/**
 * `SchemaField[]` → JSON Schema。
 *
 * 只翻译我们声明支持的部分：type / enum / range / required / default。
 * `secret` 不是 JSON Schema 概念，保留为自定义键（`x-secret`），提交时不回显值。
 */
const schema = computed<JsonSchema7>(() => {
  const properties: Record<string, JsonSchema7> = {};
  const required: string[] = [];
  for (const field of props.fields) {
    const entry: JsonSchema7 = {
      type: field.type === "integer" || field.type === "number" ? "number" : field.type,
      title: field.label || field.key,
    };
    if (field.description) entry.description = field.description;
    if (field.enum?.length) entry.enum = field.enum;
    if (field.min != null) entry.minimum = field.min;
    if (field.max != null) entry.maximum = field.max;
    if (field.default !== undefined) entry.default = field.default;
    if (field.type === "integer") entry.multipleOf = 1;
    // `x-secret` 是自定义键（JSON Schema 允许 x- 前缀扩展），
    // 类型上没有它，显式断言比污染类型定义干净。
    if (field.secret) (entry as Record<string, unknown>)["x-secret"] = true;
    properties[field.key] = entry;
    if (field.required) required.push(field.key);
  }
  return { type: "object", properties, ...(required.length ? { required } : {}) };
});

/** UI Schema：一列排下来，字段顺序与插件声明一致。 */
const uischema = computed(() => ({
  type: "VerticalLayout",
  elements: props.fields.map((field) => ({
    type: "Control",
    scope: `#/properties/${field.key}`,
    options: { ...(field.secret ? { format: "password" } : {}) },
  })),
}));

async function save(): Promise<void> {
  saving.value = true;
  notice.value = "";
  failed.value = false;
  try {
    await api.post(`/api/plugins/${encodeURIComponent(props.pluginId)}/config`, {
      config: data.value,
    });
    notice.value = t("console.plugins.configSaved");
    emit("saved");
  } catch (error) {
    failed.value = true;
    // 后端给的是「哪个字段为什么不对」，直接显示，别包一层通用错误。
    notice.value = error instanceof Error ? error.message : String(error);
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <div>
    <JsonForms
      :data="data"
      :schema="schema"
      :uischema="uischema"
      :renderers="vuetifyRenderers"
      @change="data = $event.data"
    />
    <v-alert v-if="notice" :type="failed ? 'error' : 'success'" variant="tonal" density="compact" class="mt-3">
      {{ notice }}
    </v-alert>
    <div class="d-flex justify-end mt-3">
      <v-btn color="primary" size="small" :loading="saving" :disabled="saving" @click="save">
        {{ saving ? t("console.plugins.saving") : t("console.plugins.saveConfig") }}
      </v-btn>
    </div>
  </div>
</template>
