<script setup lang="ts">
/**
 * 登录门。
 *
 * 表单校验、错误提示、按钮态全部用 Vuetify 组件：`v-form` 负责必填校验，
 * `v-alert` 负责错误态，`v-btn :loading` 负责忙碌态。不再手写这些状态。
 *
 * 行为与旧页面一致：拿不到令牌时不自动跳转（跳转会丢 `?locale=`，也会让
 * 「为什么被踢出来」不可见），就地给一个输入框。
 */

import { ref } from "vue";
import { api, ApiError, hasToken, storeToken } from "./api";
import { useI18n } from "./i18n";
import { icons } from "./icons";

const emit = defineEmits<{ authed: [] }>();

const { t } = useI18n();
const form = ref<{ validate: () => Promise<{ valid: boolean }> } | null>(null);
const value = ref("");
const busy = ref(false);
const error = ref("");
const detail = ref("");

async function submit(): Promise<void> {
  error.value = "";
  detail.value = "";
  const result = await form.value?.validate();
  if (!result?.valid) return;
  busy.value = true;
  storeToken(value.value);
  try {
    // 用最轻的鉴权接口验证：200 就算令牌有效。
    await api.get("/api/status");
    if (!hasToken()) {
      error.value = t("console.ui.login.storageBlocked");
      return;
    }
    emit("authed");
  } catch (caught) {
    if (caught instanceof ApiError && caught.status === 401) {
      error.value = t("console.ui.login.rejected");
    } else if (caught instanceof ApiError) {
      error.value = caught.message;
      detail.value = caught.detail ?? "";
    } else {
      error.value = String(caught);
    }
  } finally {
    busy.value = false;
  }
}
</script>

<template>
  <v-app>
    <v-main>
      <v-container class="fill-height" fluid>
        <v-row align="center" justify="center">
          <v-col cols="12" sm="8" md="5" lg="4">
            <v-card variant="outlined" class="pa-6">
              <div class="d-flex align-center mb-4">
                <h1 class="text-h6">{{ t("console.ui.login.title") }}</h1>
                <v-btn icon variant="text" size="x-small" class="ms-2" :aria-label="t('console.ui.info.more')">
                  <v-icon :icon="icons.info" size="small" />
                  <v-tooltip activator="parent" location="bottom" max-width="420">
                    {{ t("console.ui.login.tip") }}
                  </v-tooltip>
                </v-btn>
              </div>

              <v-form ref="form" @submit.prevent="submit">
                <v-text-field
                  v-model="value"
                  :label="t('console.ui.login.label')"
                  :placeholder="t('console.ui.login.placeholder')"
                  type="password"
                  autocomplete="current-password"
                  prepend-inner-:icon="icons.key"
                  :rules="[(v: string) => !!v.trim() || t('console.ui.login.empty')]"
                />
                <v-alert
                  v-if="error"
                  type="error"
                  variant="tonal"
                  density="compact"
                  class="mt-3"
                  :text="error"
                />
                <div v-if="detail" class="text-caption text-medium-emphasis mt-1 break-all">
                  {{ detail }}
                </div>
                <v-btn
                  type="submit"
                  color="primary"
                  block
                  class="mt-4"
                  :loading="busy"
                  :disabled="busy"
                >
                  {{ busy ? t("console.ui.login.checking") : t("console.ui.login.submit") }}
                </v-btn>
              </v-form>
            </v-card>
          </v-col>
        </v-row>
      </v-container>
    </v-main>
  </v-app>
</template>
