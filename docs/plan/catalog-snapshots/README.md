# SnowLuma 目录快照与实验记录

`Verified { source }` 必须指向可复现的资产。本目录存放两类文件：

- `snowluma-<date>-<count>-actions.json`：SnowLuma 动作目录快照，形状是
  `[{ name, readOnly, summary, params }]`。
- `snowluma-<date>-readonly-experiments.json`：真机只读实验记录，含请求参数、
  retcode、返回体关键字段与结论。

## snowluma-2026-09-29-190-actions.json

- **来源**：上游 Node bridge 仓库 `/home/nixos/Documents/code/qq-bridge/_snowluma-catalog.json`
  原样复制；该文件在该仓库 release `v0.1.7`（commit `ce726bb`，2026-09-27 21:39 +0800）
  引入，文件 mtime 为 2026-09-29 21:54 +0800。
- **导出方式**：上游仓库没有记录生成脚本，也没有版本字段；它不是 Rust 适配器
  生成的，形状与运行时动作目录一致（190 个动作）。不能声称它由某个 SnowLuma
  版本自动导出。
- **版本参照**：2026-11-05 运行中的 SnowLuma 实例返回
  `app_name=SnowLuma` / `app_version=2.2.1-node` / `protocol_version=v11`。
  这只是运行实例版本，**不是快照的版本证明**。
- **用途**：批次 9 审计的 52 个 `Verified` 出处；source 写成
  `docs/plan/catalog-snapshots/snowluma-2026-09-29-190-actions.json#<action>`。
- **过期规则**：升级 SnowLuma 后必须重新导出快照，逐条复核 `Verified` 的
  action 是否仍存在、参数是否仍一致；否则依据过期，应降回 `Unverified`。

## snowluma-2026-11-05-readonly-experiments.json

- **对象**：运行中的 SnowLuma `2.2.1-node`（账号 `3683924053`），端点
  `http://127.0.0.1:53000`。
- **方式**：桥对 `Unverified` op 的 `platform_call` 一律拒绝，console 也没有
  tool_call 入口；只读探针直接打同一实例的 OneBot HTTP API，使用桥配置里的
  同一 token。写入/破坏性动作未执行。
- **脱敏**：QQ 空间正文只记 `content_chars`，不落原文；列表只记 tid。
