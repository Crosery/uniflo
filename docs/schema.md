# Wire schema v1

> 所有 harness 归一后的唯一对外格式：`Session`、`Event`、`Envelope`，以及接口响应类型。真源 `crates/uniflo-schema/src/`。

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

### status

- `work`：agent 正在产出（收到用户输入、在思考、调工具、流式输出中）。
- `idle`：回合结束、进程退出、或长时间无活动。
- 推导规则见 `docs/architecture.md#状态机`；所有 harness 同一套规则。

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
| `usage` | `input`, `output`, `cache_read`, `cache_write`, `reasoning` | token 用量（单次调用） |
| `system` | `subtype`, `text` | harness 通知：通用的有 `compact`、`error`、`api_error`、`command`、`bash`、`patch`；其余 subtype 透传 harness 原始类型 |

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

## 全文检索

`GET /v1/search` 与 `uniflo grep --json` 的响应，真源 `crates/uniflo-schema/src/search.rs`。语法与排序见 `docs/search.md#全文检索`。

`SearchResponse`：

| 字段 | 类型 | 说明 |
|---|---|---|
| `q` | string | 原样回显的检索词 |
| `filter` | string? | 原样回显的会话过滤 |
| `order` | `"relevance"` \| `"recent"` | `recent` 表示有少于 3 个字符的词，改按时间倒序 |
| `total` | number | 命中的会话总数（分页前） |
| `offset`, `limit` | number | 实际生效的分页参数 |
| `indexing` | bool | 索引仍在构建或追赶，结果可能不全 |
| `progress` | `{done, total, events}` | 已建完的会话数 / 会话总数 / 已入索引的事件数 |
| `results` | `SearchSession[]` | 当前页 |

`SearchSession`：`session`（会话 key）、`harness`、`title?`、`cwd?`、`updated_at?`、`score`（越大越相关，`recent` 时为 0）、`hits`（最多 3 条，最好的在前）。

`SearchHit`：`event`（事件 id，传给 `events?around=`）、`kind`、`ts`、`snippet`（高亮区间以 `\u0002` 开始、`\u0003` 结束，常量 `HIGHLIGHT_START` / `HIGHLIGHT_END`）。

`FtsStatus`（`GET /v1/stats` 的 `fts`）：`indexing`、`progress`、`path`（索引文件）、`bytes`（索引文件含 WAL 的磁盘占用）、`rebuilt`（本次启动因格式标签变化或文件损坏而重建）、`build_ms?`（从空索引开始的最近一次全量构建耗时）、`errors`、`last_error?`。

## 示例

```json
{"type":"event","seq":812,"event":{"id":"msg_01:2","session":"claude:4f1c…","ts":1790000000123,"pos":53311,"kind":"tool_call","call_id":"toolu_01","name":"Bash","input":{"command":"cargo test"}}}
{"type":"session","seq":813,"session":{"key":"claude:4f1c…","harness":"claude","id":"4f1c…","source":"/Users/me/.claude/projects/-x/4f1c….jsonl","updated_at":1790000000123,"status":"work","status_since":1790000000001,"status_reason":"tool_call","pid":55935}}
```
