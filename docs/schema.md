# Wire schema v1

> 所有 harness 归一后的唯一对外格式：`Session`、`Event`、`Envelope`。真源 `crates/uniflo-schema/src/lib.rs`。

状态：`current` · 更新：2026-10-09

## 兼容规则

- v1 只增不改：会新增可选字段和新的 `kind` / `type` 取值；客户端必须忽略不认识的字段和取值。
- 删除、改名、改语义 → `SCHEMA_VERSION` 加一，`/v1/health` 的 `schema` 字段随之变化。
- 时间一律 Unix 毫秒（`i64`）；可选字段缺省时不输出。

## Session

一个 harness 的一次对话。`key = "<harness>:<id>"` 全局唯一。

| 字段 | 类型 | 说明 |
|---|---|---|
| `key` | string | `claude:4f1c…`，所有接口的会话主键 |
| `harness` | string | harness id，见 `GET /v1/harnesses` |
| `id` | string | harness 内的会话 id |
| `parent` | string? | 父会话 `key`（子代理、fork） |
| `title` | string? | harness 自带或生成的标题，按优先级取最高者 |
| `cwd` | string? | 工作目录 |
| `model` | string? | 最近使用的模型 |
| `preview` | string? | 第一条人类输入，截断 |
| `source` | string | 会话所在文件路径或 `sqlite://<db>#<id>` |
| `started_at` | i64? | 开始时间 |
| `updated_at` | i64 | 最后活动时间 |
| `status` | `"work"` \| `"idle"` | 见下文 |
| `status_since` | i64 | 进入当前状态的时间 |
| `status_reason` | string? | `user_message`、`tool_call`、`turn_end`、`stale`、`exited`、`live` 等 |
| `pid` | u32? | 当前附着的进程（能探测到时） |
| `usage` | object? | 会话合计，见下文 [Session.usage](#sessionusage)；后台用量索引读完该会话前不输出 |

### status

- `work`：agent 正在产出（收到用户输入、在思考、调工具、流式输出中）。
- `idle`：回合结束、进程退出、或长时间无活动。
- 推导规则见 `docs/architecture.md#状态机`；所有 harness 同一套规则。

### Session.usage

该会话全部 `usage` 事件的合计，有新的 usage 事件时随 `session` envelope 实时更新。

| 字段 | 类型 | 说明 |
|---|---|---|
| `steps` | u64 | usage 事件数（一次模型调用一步） |
| `input` `output` `cache_read` `cache_write` `reasoning` | u64 | 各步之和，口径同 [usage](#usage-口径) |
| `cost_usd` | f64? | 已定价各步的费用之和；一步都定不了价时为 `null`，不是 0 |
| `unpriced_steps` | u64 | 无自报金额、模型也不在价格目录里的步数 |
| `last_context_tokens` | u64 | 最后一步的 `input + cache_read + cache_write` |
| `context_limit` | u64? | 最后一步模型在目录里的上下文上限 |

## Event

会话里的一条归一化记录。`kind` 决定其余字段（平铺在同一对象里）。

公共字段：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 会话内稳定；**同 id 再次出现表示覆盖**（流式更新、工具状态推进） |
| `session` | string | 所属会话 `key` |
| `ts` | i64 | 时间；个别 harness 不记录时间时为 0 |
| `pos` | u64? | 分页游标，`GET …/events?before=<pos>` 取更早的 |
| `partial` | bool | 仍在流式生成，之后会有同 id 的完整版本 |
| `truncated` | bool | 文本被网关 `max_text` 截断 |
| `kind` | string | 下表之一 |

| kind | 字段 | 含义 |
|---|---|---|
| `user_message` | `text`, `synthetic?` | 用户输入；`synthetic=true` 表示 harness 注入（命令输出、提醒、hook 反馈） |
| `assistant_message` | `text`, `model?` | 模型可见回复 |
| `reasoning` | `text` | 思考内容（被加密/隐藏时为 `[redacted]`） |
| `tool_call` | `call_id`, `name`, `input` | 工具调用；`input` 是 JSON（harness 存成字符串的参数会先尝试按 JSON 解析） |
| `tool_result` | `call_id`, `name?`, `output`, `is_error?` | 工具结果，按 `call_id` 对应调用 |
| `turn_start` | — | 回合开始（harness 有显式标记时） |
| `turn_end` | `reason?` | 回合结束；`reason` 透传 harness 原始原因（`end_turn`、`stop`、`interrupted`、`turn_duration`…），各家取值不同，不要依赖具体值 |
| `usage` | `input`, `output`, `cache_read`, `cache_write`, `reasoning`, `model?`, `cost_usd?` | 一次模型调用的 token 用量，口径见下 |
| `system` | `subtype`, `text` | harness 通知：通用的有 `compact`、`error`、`api_error`、`command`、`bash`、`patch`；其余 subtype 透传 harness 原始类型 |

### usage 口径

所有适配器按同一口径填写（各 harness 的换算见 `docs/adapters.md#usage-口径`）：

| 字段 | 口径 |
|---|---|
| `input` | 未命中缓存的输入 token，不含 `cache_read` 与 `cache_write` |
| `output` | 计费输出 token，**包含**思考 token |
| `reasoning` | `output` 中属于思考的部分，仅供参考，不重复计费，不超过 `output` |
| `cache_read` | 命中缓存读取的 token |
| `cache_write` | 写入缓存的 token |
| `model` | 这一步实际使用的模型；缺省时引擎用该会话最近一条 assistant 消息或元数据里的模型补上 |
| `cost_usd` | harness 自报的这一步金额（USD）；没有自报的 harness 不输出，自报 ≤ 0 视为未报 |

一步的上下文占用 = `input + cache_read + cache_write`。费用计算见 `docs/decisions/ADR-0010-价格目录同步与费用口径.md`。

## 用量与价格接口的类型

`/v1/usage`、`/v1/sessions/{key}/usage`、`/v1/models`、`/v1/pricing` 的响应类型在 `crates/uniflo-schema/src/usage.rs`，同样只增不改。金额单位 USD，价格单位 USD / 百万 token；对订阅用户都是"API 等价成本"，不是账单。

| 类型 | 用途 | 要点 |
|---|---|---|
| `UsageReport` | `/v1/usage` | `group_by`、`since`、`until`、`tz`、`under`、`rows[]`、`totals`、`pricing{fetched_at, stale}`、`indexing{ready, done, total}` |
| `UsageRow` | 一行聚合 | `key`（机器键）、`label`（显示名）、`sessions`、`steps`、`prompts`、五项 token、`cost_usd?`、`unpriced_steps`；各行之和等于 `totals` |
| `SessionUsageDetail` | `/v1/sessions/{key}/usage` | `steps[]`（`StepUsage`）、`turns[]`（`TurnUsage`）、`totals`（`SessionUsage`） |
| `StepUsage` | 一步 | `event`、`ts`、`turn`、`model?`、五项 token、`cost_usd?`、`cost_source?`（`harness` / `catalog` / `approx`）、`context_tokens`、`context_limit?`、`context_pct?` |
| `TurnUsage` | 一回合 | `turn`、`started_at`、`prompts`、`steps`、五项 token、`cost_usd?`、`unpriced_steps` |
| `ModelUsage` | `/v1/models` 一项 | `model`（原始名）、`match`（`exact` / `approx` / `none`）、`catalog_id?`、`provider?`、`prices[]`、`context_limit?`、`output_limit?`、`sessions`、`steps`、`cost_usd?`、`unpriced_steps` |
| `PriceSegment` | 一段价格 | `from`、`until?`（epoch ms，`[from, until)`）、`input`、`output`、`cache_read`、`cache_write?`、`tiers[]`（`{above, input, output, cache_read, cache_write?}`） |
| `PricingStatus` | `/v1/pricing` | `source`、`fetched_at?`、`stale`、`error?`、`models`、`overrides`、`last_attempt?`、`pending`、`sync_enabled` |

## Envelope

实时流的一行（NDJSON 一行 / SSE 一个事件 / WebSocket 一帧），按 `type` 区分：

| type | 字段 | 客户端动作 |
|---|---|---|
| `hello` | `seq`, `version`, `server` | 连接建立；`seq` 是当前最新序号 |
| `session` | `seq`, `session` | 按 `session.key` 整体替换 |
| `event` | `seq`, `event` | 按 `(event.session, event.id)` upsert |
| `removed` | `seq`, `key` | 删除会话（源文件被删） |
| `lagged` | `seq`, `missed` | 订阅落后、有丢失：用 REST 重新拉快照 |

`seq` 在一次守护进程运行内严格递增。断线重连带 `since=<最后收到的 seq>`（SSE 也认 `Last-Event-ID`）补齐缺口。守护进程重启后 `seq` 从 0 开始：`hello.seq` 小于本地记录时，重新拉快照。

## 示例

```json
{"type":"event","seq":812,"event":{"id":"msg_01:2","session":"claude:4f1c…","ts":1790000000123,"pos":53311,"kind":"tool_call","call_id":"toolu_01","name":"Bash","input":{"command":"cargo test"}}}
{"type":"session","seq":813,"session":{"key":"claude:4f1c…","harness":"claude","id":"4f1c…","source":"/Users/me/.claude/projects/-x/4f1c….jsonl","updated_at":1790000000123,"status":"work","status_since":1790000000001,"status_reason":"tool_call","pid":55935}}
```
