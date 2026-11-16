# ADR-0031 控制台前端：Vuetify + JSONForms，产物内嵌进二进制

来源：用户裁定（2026-11-16）：
1. 「前端用一些前端框架完成，可以构建后内嵌，前后端分离」；
2. 「很多文字应该收缩到 info 那个圆形的按钮而不是直接塞在页面上」；
3. **「我希望能一个最佳实践，而不是我们自己手写以减少依赖等（这样做会导致很多问题，对
   agent 和人都是很大的负担）」**——这条推翻了本 ADR 的第一版。

第一版（同日更早）选了「Vue 3 + 手写组件与 CSS」：装是装了框架，但表格、表单、弹层、
状态块、分页、校验、断点降级全是自己写的（765 行手写 CSS + 11 个自造组件）。用户点出这
仍然是在「手写以减少依赖」，只是把 HTML 换成了 Vue。第二版按「最佳实践」重做：**组件库与
动态表单都交给成熟库**，我们只写业务胶水。

## 这条决定了什么行为

旧控制台是 `crates/bridge/src/console.html`：2419 行单文件，内联 CSS + 内联 JS，
零构建、零依赖，`include_str!` 进二进制。它做对了设计令牌、深浅色、触控尺寸、i18n 属性，
但代价是：

1. **文案铺在页面上**。56 处 `class="muted"` + 31 处 `class="hint"`，很多是整段解释。
   想加一句说明最省事的写法就是直接写进 HTML，于是页面越来越像说明书。
2. **没有组件边界**。七个 tab 共享一个全局作用域，改一处要在 2400 行里找。
3. **动态表单没有落点**。插件自描述（`plugin_manifest.rs` 的 `form_schema()`）已经有
   schema，但手写 JS 拼不出来——`J3` 的插件配置表单是纯字符串模板。
4. **无类型**。`/api/*` 的响应形状只活在读代码的人的脑子里，后端改字段前端静默失效
   （这一轮真踩到：`busy_sessions` 是数组不是数字，页面显示 `[]`）。

新行为：

- 前端源码在 `web/`（Vite + Vue 3 + TypeScript）。
- 构建产物在 `crates/bridge/src/console_web/`（`index.html` / `app.js` / `app.css`，
  **文件名固定、无内容哈希**），由 `include_bytes!` 内嵌。
- 服务路径 `/next`，与旧页面**并存**。旧 `console.html` 一字未动，仍是 `GET /`。
- 文案纪律由组件承担：`CardShell` 只接受 `tipKey`/`tip`（交给 `<InfoTip>`），正文只放数据。

## 最终选型（第二条裁定之后）

| 层 | 选择 | 版本 | 为什么是它 |
|---|---|---|---|
| 框架 | Vue 3 | 3.5.45 | 已有 Vite 链；SFC 可读性对多 agent 接手友好 |
| 组件库 | **Vuetify** | 3.13.7 | **唯一有官方 JSONForms 渲染器的 Vue 组件库**；无运行时依赖（只 peer vue）；`v-data-table` 自带响应式、分页、空态 |
| 动态表单 | **@jsonforms/vue + @jsonforms/vue-vuetify** | 3.8.0 | 官方渲染器，JSON Schema → Vuetify 控件（enum/范围/必填/布尔）零手写 |
| 服务端状态 | **@tanstack/vue-query** | 5.107.1 | 缓存、失效、重试、窗口聚焦刷新、`enabled` 条件查询，替掉手写的 120 行 `useResource` |
| 路由 | **vue-router** | 4.6.5 | hash 模式；4.x 而非 5.x——5.x 的 peer 强制 pinia + @pinia/colada，本控制台不需要那种状态层 |
| 构建 | Vite + vite-plugin-vuetify | 8.3.8 / 2.1.3 | 与 docs 同一套工具链 |

实测体积（gzip）：`app.js` 346.58 kB、`app.css` 66.71 kB。用户明说「大小不是需要考虑的
内容，只要不是特别大」，且这个二进制本身 170 MB 量级，所以体积不构成选型依据。

### 否决理由（按权重）

- **Svelte 5 + `@sjsf/form`**：体积更小（17 kB vs 27 kB 裸框架），但 `@sjsf/form` 171 star、
  README 自称「非官方移植」，而动态 schema 表单是唯一被点名要求预留的能力——不能建在单点
  依赖上。另外 runes 语法与训练数据里大量 Svelte 4 写法冲突，对「很多 agent 接手过」的仓库
  是真实成本。
- **Naive UI / Element Plus / Ant Design Vue / PrimeVue**：实测无官方 JSONForms 渲染器。
  要么手写渲染器（回到原点），要么用 JSONForms 的 vanilla 渲染器（样式与组件库不一致，
  「看起来像两个站点」）。
- **Vuetify 4.x**：`@jsonforms/vue-vuetify` 的 peer 是 `vuetify ^3.11.0`，必须锁 3.x。
- **`vite-plugin-vuetify` 的 `autoImport: true`**：实测漏了 `<v-app-bar-title>` 的样式
  （该组件 CSS 没进产物，页面布局塌成一列）。本控制台体积不是约束，所以改为全量注册
  `vuetify/components` + `vuetify/styles`，最不容易坏。
- **VitePress**：SSG 面向 content-centric 站点，实测最小站点 24 个文件 + 76 kB 首屏，
  还带字体预加载。它留在 `docs/` 是对的。

### 依赖清单里几个必须锁版本的坑（实测）

- `maska` 必须 **2.x**：`@jsonforms/vue-vuetify` 用 `vMaska`，而 maska 3.x 删了这个导出，
  构建会报 `"vMaska" is not exported`。
- `@jsonforms/vue-vuetify` **没有 default export**，渲染器集合叫 `vuetifyRenderers`。
- `@jsonforms/core` 的 `JsonSchema` 是 `JsonSchema4 | JsonSchema7` 联合，写表单 schema 时
  直接用它的 `JsonSchema7`，别从 `json-schema` 包引一份不同源的——两套类型对
  `additionalItems` 定义不同，混用必然类型冲突。
- `vuetify/locale` 的子路径没有类型声明，走包根命名导出 `import { en, zhHans } from "vuetify/locale"`。

## 手写量对比（这就是第二条裁定的验收口径）

| | 第一版（自造组件） | 第二版（Vuetify + JSONForms） |
|---|---|---|
| 手写 CSS | 765 行（含 tokens + base + 11 个组件的 scoped） | **17 行**（一个等宽字体、两个下拉宽度） |
| 自造组件 | 11 个（表格/表单/弹层/状态块/卡片/分页…） | 3 个（PageCard 卡片头、QueryState 三态映射、PluginConfigForm schema 翻译） |
| 表格 | 手写 `<table>` + `≤720px` 转卡片的媒体查询 | `v-data-table`（响应式、分页、空态、排序自带） |
| 表单控件 | 手写 input/select/checkbox + 校验 | `v-text-field` / `v-select` / `v-switch` + JSONForms |
| 弹层 | 手写定位与视口钳制 | `v-tooltip`（自带定位、滚动跟随、键盘可达） |
| 状态层 | 手写 120 行 `useResource` | `@tanstack/vue-query` |

保留的 3 个自造组件不是"重造轮子"，而是**业务语义的翻译层**：`PageCard` 把「标题 + ⓘ +
操作区」这个在七页必须一致的形状固定下来（并强制解释走 ⓘ）；`QueryState` 把查询三态映射到
Vuetify 的进度条与告警；`PluginConfigForm` 把我们扁平的 `SchemaField[]` 翻译成 JSON Schema
+ UI Schema 一对。这三样没有现成库能做，也不该由库来做。

## 为什么产物提交进仓库，而不是在 `nix build` 里构建

- `nix build` 现在是**纯 Rust + `--offline`**。加前端构建会给每次 `nix build` 拉进
  node + pnpm 闭包，代价远大于收益。
- nix 的 `fetchPnpmDeps` 方案可行，但要维护 lockfile hash，且 lockfile 一变就得手改
  （nixpkgs 有过 hash 不重算导致不重建的 issue）。等前端真的频繁改动了再迁。

**代价与它的守卫**：产物会与源码漂移。所以 `deploy/verify.sh` 新增 `web` 阶段
（`deploy/verify-console-web.sh`）：在临时目录真跑一次 `vite build`，逐字节比对三个产物。
这条守卫**正反双向验证过**：改一个会进产物的 CSS 规则不重建 → 红并报告字节差；还原后 → 绿。

## 前后端分离的边界（为自描述接口预留）

这一条是用户点名要预留的。当前落点：

- **服务端**：`plugin_manifest.rs` 的 `PluginManifest::form_schema()` 已经产出
  `{config, resources, actions}`，字段含 `type` / `enum` / `min` / `max` / `required` /
  `secret`。这是插件**自报**的，core 只照着执行。
- **客户端**：`web/src/components/SchemaForm.vue` 是**唯一按字段名分支**的地方，分支依据是
  `SchemaField.type`，不是任何具体插件的 key。新增插件不需要动它。
- **类型**：`web/src/types.ts` 的 `PluginSchema` / `SchemaField` / `SchemaResource` /
  `SchemaAction` 直译服务端结构，注释标明出处文件。

将来扩成 OData 式的资源/关系模型时，改的是「`SchemaForm` 的渲染器」与「`SchemaResource`
的形状」，不是七个 tab 的任何一个。

## 为什么 `/next` 并存而不是直接替换

- 旧页面有一批静态契约测试（`console_html_defines_distinct_dependency_state_rendering`
  等）与已验收的运维习惯；切换该是**一次单独的行动**，不混在重写里一起发生。
- 切换方法只有一处：`crates/bridge/src/console_web.rs` 的 `NEXT_UI_PREFIX`。

## 缓存策略为什么是 `no-cache` 而不是 `immutable`

产物文件名固定（Rust 侧在编译期要知道嵌哪几个文件），没有内容哈希。所以：

- 入口页 `Cache-Control: no-store`；
- 资源 `Cache-Control: no-cache`（可缓存，但每次回源校验）。

**改前端 → 重建 → 刷新即最新**，不需要用户清缓存。代价是每次刷新多一次 304 往返；
控制台是内网运维页，这个代价值得。

## 已知未做

- 只做了「看清楚」：插件配置表单可保存，但**会话/权限的结构化编辑仍在旧页面**。
  两套并存期间，某些操作要去 `GET /` 做。
- 未做响应式布局的真机矩阵验证（只验了 1236px 与 CSS 断点定义）。
- `feedback` 页只读，回复入口未做（回复会通知原会话，写错话的代价比看错状态大）。
