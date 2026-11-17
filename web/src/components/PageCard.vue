<script setup lang="ts">
/**
 * 页面卡片：标题 + ⓘ + 右侧操作区 + 内容插槽。
 *
 * 这个组件是「文字不进页面」的**唯一入口**。它只接受 `tipKey`（i18n 词条），
 * 不接受自由段落——想让一段解释铺在页面上，得先绕过这个 API，review 时一眼可见。
 * 解释由 Vuetify 的 `v-tooltip` 呈现（自带定位、滚动跟随、键盘可达）。
 *
 * 为什么保留这个薄封装而不是每页直接写 `v-card`：卡片头的三件套（标题、
 * ⓘ、操作区）在七个页面里必须长得一样，写成组件才不会漂。
 */

import { computed } from "vue";
import { useI18n } from "../i18n";
import { icons } from "../icons";

const props = withDefaults(
  defineProps<{
    /** 标题的 i18n key。 */
    titleKey?: string;
    /** 已翻译的标题（动态内容用）。 */
    title?: string;
    /** 解释的 i18n key，交给 ⓘ。 */
    tipKey?: string;
    /** 已翻译的解释（动态内容用）。 */
    tip?: string;
  }>(),
  {},
);

const { t } = useI18n();

const heading = computed(() => (props.titleKey ? t(props.titleKey) : props.title ?? ""));
const tipText = computed(() => (props.tipKey ? t(props.tipKey) : props.tip ?? ""));
</script>

<template>
  <v-card variant="outlined" class="mb-4">
    <v-card-title v-if="heading || tipText || $slots.actions" class="d-flex align-center ga-2 py-3">
      <span class="text-body-1 font-weight-medium">{{ heading }}</span>
      <v-btn
        v-if="tipText"
        icon
        variant="text"
        size="x-small"
        :aria-label="t('console.ui.info.more')"
      >
        <v-icon :icon="icons.info" size="small" />
        <v-tooltip activator="parent" location="bottom" max-width="460">
          <span style="white-space: pre-line">{{ tipText }}</span>
        </v-tooltip>
      </v-btn>
      <v-spacer />
      <div v-if="$slots.actions" class="d-flex align-center flex-wrap ga-2">
        <slot name="actions" />
      </div>
    </v-card-title>
    <v-divider v-if="heading || $slots.actions" />
    <v-card-text>
      <slot />
    </v-card-text>
  </v-card>
</template>
