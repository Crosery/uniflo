# ADR-0003 · wire schema 只增不改

> 对外只有一套 `Session` / `Event` / `Envelope` 格式；v1 内只允许新增可选字段与新取值。

状态：`accepted` · 更新：2026-10-02

## 背景

第三方桌面端 / 网页端要用一套解析逻辑对接所有 harness，且不能因为 Uniflo 升级而崩。

## 决策

- 契约真源 `crates/uniflo-schema`，文档 `docs/schema.md`，二者同提交更新。
- 事件按 `kind` 区分，字段平铺；同 `id` 再次出现即覆盖（流式）。
- 状态只有 `work` / `idle` 两值，原因放 `status_reason`（自由文本，可扩展）。
- 破坏性变更必须升 `SCHEMA_VERSION` 并新增 ADR；客户端通过 `/v1/health.schema` 与 `hello.version` 检测。

实施状态：已落地，`SCHEMA_VERSION = 1`。

## 否决项

- 透传各 harness 原始记录：客户端要写 N 套解析。
- 细粒度状态（thinking / tool_running / waiting_user…）：各 harness 信号不一致，无法统一保证正确；需要时从事件流自行推导。

## 后果

- 客户端解析稳定；新 harness 不影响客户端。
- harness 特有信息只能以 `system.subtype` 或新的可选字段承载。

## 复议触发

需要删除或改变既有字段语义时（升 v2）。
