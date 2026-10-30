# Wire schema v1

> 所有 harness 归一后的唯一对外格式：`Session`、`Event`、`Envelope`，以及接口响应类型。真源 `crates/uniflo-schema/src/`。

状态：`current` · 更新：2026-10-10

## 兼容规则

- v1 只增不改：会新增可选字段和新的 `kind` / `type` 取值；客户端必须忽略不认识的字段和取值。
- 删除、改名、改语义 → `SCHEMA_VERSION` 加一，`/v1/health` 的 `schema` 字段随之变化。
- 时间一律 Unix 毫秒（`i64`）；可选字段缺省时不输出。
- 对象的键顺序不是契约。`tool_call` 的 `input` 这类透传 JSON 按 harness 原文的键顺序输出（`serde_json` 开启了 `preserve_order`）。

## Session

一个 harness 的一次对话。`key = "<harness>:<id>"` 全局唯一。

| 字段 | 类型 | 说明 |
|---|---|---|
| `key` | string | `claude:4f1c…`，所有接口的会话主键 |
| `harness` | string | harness id，见 `GET /v1/harnesses` |
| `id` | string | harness 内的会话 id |
| `parent` | string? | 父会话 `key`（子代理、fork） |
| `parent_call` | string? | 启动本会话的那次工具调用（父会话里某个 `tool_call` 的 `call_id`），harness 记录了才有。目前只有 Claude 子代理：读 `agent-<id>.jsonl` 旁边 `agent-<id>.meta.json` 的 `toolUseId`（只读） |
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
| `family` | object? | 本会话加上全部后代会话的合计，见下文 [Session.family](#sessionfamily)；没有后代会话时不输出 |
| `archived` | bool | 源文件已被清理（移入回收站），事件来自 Uniflo 的精简归档，`source` 指向归档文件、状态为 idle（`status_reason` 为 `archived`）；源文件从回收站还原后消失。缺省为 false，见 `docs/api.md#会话清理` |

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
| `unpriced_steps` | u64 | 无自报金额、模型也不在价格目录里的步数；token 全为 0 的步骤不算（没有可计价的东西，费用为 0） |
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
| `truncated` | bool | 文本被截断：网关按 `max_text` 截断，或写入精简归档时被截短、去掉了内嵌二进制（见 [会话清理](#会话清理)） |
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

### Session.family

本会话**加上**经 `parent` 链递归得到的全部后代会话（子代理的子代理也算）的合计。只在至少有一个后代会话时输出。

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessions` | u64 | 后代会话数，不含本会话；还没读到用量的后代也算一个 |
| `steps` `input` `output` `cache_read` `cache_write` `reasoning` `unpriced_steps` | u64 | 本会话与各后代的 `usage` 之和；`usage` 缺失（用量尚未索引）的会话按 0 计 |
| `cost_usd` | f64? | 已定价金额之和；全部未定价时为 `null` |

任一成员会话的 `usage` 变化、后代会话增减时，受影响的祖先会话（以及成员本身）的 `family` 随之重算，并和 `usage` 一样经 `session` envelope 推送。

## 用量与价格接口的类型

`/v1/usage`、`/v1/sessions/{key}/usage`、`/v1/models`、`/v1/pricing` 的响应类型在 `crates/uniflo-schema/src/usage.rs`，同样只增不改；`/v1/sessions/{key}/trajectory` 的在 `crates/uniflo-schema/src/trajectory.rs`（见 [会话链路的类型](#会话链路的类型)）。金额单位 USD，价格单位 USD / 百万 token；对订阅用户都是"API 等价成本"，不是账单。

| 类型 | 用途 | 要点 |
|---|---|---|
| `UsageReport` | `/v1/usage` | `group_by`、`since`、`until`、`tz`、`under`、`rows[]`、`totals`、`pricing{fetched_at, stale}`、`indexing{ready, done, total}` |
| `UsageRow` | 一行聚合 | `key`（机器键）、`label`（显示名）、`sessions`、`steps`、`prompts`、五项 token、`cost_usd?`、`unpriced_steps`；各行之和等于 `totals` |
| `SessionUsageDetail` | `/v1/sessions/{key}/usage` | `steps[]`（`StepUsage`）、`turns[]`（`TurnUsage`）、`totals`（`SessionUsage`） |
| `StepUsage` | 一步 | `event`、`ts`、`turn`、`model?`、五项 token、`cost_usd?`、`cost_source?`（`harness` / `catalog` / `approx`）、`cost_parts?`、`price?`、`context_tokens`、`context_limit?`、`context_pct?` |
| `CostParts` | `StepUsage.cost_parts` | `{input, output, cache_read, cache_write}`：这一步金额按 token 类别的拆分（USD），四项之和等于 `cost_usd`（误差 < 1e-9） |
| `StepPrice` | `StepUsage.price` | `{input, output, cache_read, cache_write, tier_above?}`：这一步实际采用的单价（USD / 百万 token），已按事件时间选好价格段、按整单 prompt 选好分档（用了分档时 `tier_above` 是该档的门槛），`cache_write` 目录缺价时为 `input × 1.25`。`cost_parts` 与 `price` 只在 `cost_source` 为 `catalog` 或 `approx` 时输出，harness 自报金额和未定价的步骤没有 |
| `SessionFamily` | `Session.family` | 见上文 [Session.family](#sessionfamily) |
| `TurnUsage` | 一回合 | `turn`、`started_at`、`prompts`、`steps`、五项 token、`cost_usd?`、`unpriced_steps` |
| `ModelUsage` | `/v1/models` 一项 | `model`（原始名）、`match`（`exact` / `approx` / `none`）、`catalog_id?`、`provider?`、`prices[]`、`context_limit?`、`output_limit?`、`sessions`、`steps`、`cost_usd?`、`unpriced_steps` |
| `PriceSegment` | 一段价格 | `from`、`until?`（epoch ms，`[from, until)`）、`input`、`output`、`cache_read`、`cache_write?`、`tiers[]`（`{above, input, output, cache_read, cache_write?}`） |
| `PricingStatus` | `/v1/pricing` | `source`、`fetched_at?`、`stale`、`error?`、`models`、`overrides`、`last_attempt?`、`pending`、`sync_enabled` |

## 会话链路的类型

`GET /v1/sessions/{key}/trajectory` 返回 `SessionTrajectory`：把一个会话的记录按"回合 → 步 → 记录"组织起来，用于逐轮、逐步查看调用链。每一步的数值与 `/v1/sessions/{key}/usage` 里的同一步完全一致。

| 类型 | 要点 |
|---|---|
| `SessionTrajectory` | `session`、`turns[]`、`children[]` |
| `TrajectoryTurn` | `turn`（与 `StepUsage.turn` 同一编号，0 = 第一个回合之前）、`event?`（开启回合的人类消息的事件 id）、`started_at`、`ended_at`（回合内最后一条记录的时间）、`prompt?`（人类消息前 300 个字符）、`steps[]`、`items[]`（回合内不属于任何一步的记录） |
| `TrajectoryStep` | `index`（整个会话内从 1 起，等于该步在 `/usage` 里的次序）、`event`（usage 事件 id）、`started_at`（上一步最后一条记录的结束时间；回合的第一步取回合开始时间）、`ts`（usage 事件时间）、`model?`、五项 token、`cost_usd?`、`cost_source?`、`cost_parts?`、`price?`、`context_tokens`、`context_pct?`、`items[]` |
| `TrajectoryItem` | 按 `kind` 区分，每种都带来源事件的 `event` 和 `ts`：`reasoning`（`chars`）、`text`（`chars`、前 200 个字符的 `preview`）、`tool`（`name`、`call_id`、`input_preview`；有结果时 `ended_at`、`is_error`、`output_preview`，没有结果时 `running: true`；`children[]` 是这次调用启动的子会话 key）、`system`（`subtype`） |
| `TrajectoryChild` | 直接子会话：`key`、`title?`（没有标题时取首条人类输入 `preview`，与会话列表一致）、`model?`、`status`、`started_at?`、`updated_at`、`usage?`、`family?`、`call_id?`、`step?`（它挂在哪次调用、哪一步，`step` 为 `TrajectoryStep.index`） |

归组规则：

- 回合由人类用户消息（或 harness 的回合开始标记）开启；harness 注入的 synthetic 消息不开新回合，也不作为记录列出。
- 两个 usage 事件之间的思考、回复、工具调用、系统记录归后一个 usage 那一步。同一个 usage 事件 id 再次出现时（Claude 把一条 API 消息的 usage 重复写在它的每个内容块上），其间的记录并入已有的那一步。回合末尾还没有 usage 的记录放在回合的 `items` 里。
- 工具结果按 `call_id` 回填到对应的调用，本身不是记录。
- 预览按字符截取（不是字节）；输入参数的预览是 JSON 紧凑文本的前 200 个字符。
- 子会话的挂接：有 `parent_call` 时按它精确对应到那次调用；没有 `parent_call` 的取开始时间之前最近的一次名称含 `agent`、`task` 或 `spawn`（不分大小写）且尚未挂接的工具调用；仍找不到时只按开始时间挂到所在的步骤（`call_id` 缺省）。`parent_call` 指向的调用不在本会话的转写里（子代理的子代理：Claude 把它们平铺在顶层会话的 `subagents/` 下；或调用已不在转写里）时不按时间猜调用，只挂到所在的步骤。
- 已清理（`archived`）的会话从归档读取。归档里同一事件 id 只留一份，Claude 式重复 usage 的后续记录会落进下一步。

## agent 接入接口的类型

`/v1/sessions/{key}/resume`、`/v1/memory`、`/v1/memory/file`、`uniflo context --json` 的响应类型在 `crates/uniflo-schema/src/agent.rs`，同样只增不改。用法见 `docs/agents.md`。

| 类型 | 用途 | 字段 |
|---|---|---|
| `ResumeInfo` | `/v1/sessions/{key}/resume`、MCP `uniflo_resume` | `key`、`harness`、`supported`、`argv`（总是输出，不支持时为 `[]`）、`cwd?`、`command?`（POSIX shell 一行）、`command_powershell?`、`reason?`（`supported=false` 时的原因） |
| `MemoryFile` | `/v1/memory` 一项 | `path`、`scope`（`global` / `project`）、`harness`（加载它的 harness；共享的 `AGENTS.md` 约定为 `agents`）、`bytes`、`updated_at`（文件修改时间） |
| `MemoryContent` | `/v1/memory/file` | `MemoryFile` 的全部字段，加 `content`（UTF-8，非法字节替换，最多 256 KB）、`truncated?`（文件比 `content` 大时为 true，否则不输出） |
| `ContextReport` | `uniflo context --json`、MCP `uniflo_context` | `project`（git 根，不在仓库里时为目录本身）、`since`、`sessions[]`（最近活动倒序） |
| `ContextSession` | 一个会话 | `key`、`harness`、`title?`、`cwd?`、`updated_at`、`preview?`（首条用户输入，最多 80 字符含省略号）、`cost_usd`（API 等价费用，未知时 `null`） |

## Harness

`GET /v1/harnesses` 的一项：一个已支持的 harness 及其实时计数。

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 会话 key 里的 harness 部分 |
| `name` | string | 显示名 |
| `roots` | string[] | 本机存在的数据根目录 |
| `sessions` | number | 当前索引里的会话数 |
| `working` | number | 其中状态为 `work` 的会话数 |
| `icon` | string? | 单色品牌图标的网关路径 `/v1/harnesses/{id}/icon.svg`；没有品牌图标时不输出，客户端显示字母块（`docs/api.md`） |

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
| `partial` | bool? | 含短词的查询在时间预算（2 s）内没扫完：`results` 是已找到的最新命中，更早的可能缺失；缺省为 false |
| `scanned_until` | i64? | 两种情况之一：含短词的查询在扫到最老的会话之前就停了（结果已够，或 `partial`）；或相关性查询确认了这一页之后，在约 250 ms 的预算内确认精确 `total` 也没扫完冷层剩下的候选。都表示最后活动时间不晚于此刻的会话可能没被数进 `total`（但不影响已返回的 `results`，那一页本身是确定的），`total` 退化为下限 |

`SearchSession`：`session`（会话 key）、`harness`、`title?`、`cwd?`、`updated_at?`、`score`（越大越相关，`recent` 时为 0）、`hits`（最多 3 条，最好的在前）。

`SearchHit`：`event`（事件 id，传给 `events?around=`）、`kind`、`ts`、`snippet`（高亮区间以 `\u0002` 开始、`\u0003` 结束，常量 `HIGHLIGHT_START` / `HIGHLIGHT_END`）。

`FtsStatus`（`GET /v1/stats` 的 `fts`）：`indexing`、`progress`、`path`（索引文件）、`bytes`（索引文件含 WAL 的磁盘占用）、`rebuilt`（本次启动因格式标签变化或文件损坏而重建）、`warming?`（启动后索引仍在合并成一个段、或在读进系统页缓存，期间某个词的第一次查询可能较慢；缺省为 false）、`build_ms?`（从空索引开始的最近一次全量构建耗时）、`errors`、`last_error?`。ADR-0011 的热/冷分层新增：`hot_days?`（当前生效的 `--fts-hot-days`，省略表示未知）、`hot_events?` / `cold_events?`（两层各自的事件行数，省略表示未知）、`cold_blocks?`（冷层压缩块数，省略表示未知）、`migrating`（后台正在把旧版 `fts-v1.sqlite` 转换进来；缺省为 false）、`reclaimed_bytes`（迁移删除旧文件释放的字节数；缺省为 0）。可靠性字段（只增不改，缺省省略）：`foreign_writes?`（持有写租约期间发现别的进程写过索引的次数，不计入 `errors`；缺省为 0）、`foreign_writers?`（见到外部写入之后，当前以写方式打开索引文件的其他进程 `FtsWriter {pid, command}`，尽力而为，取不到时省略）。`indexing` 只在有会话尚未首次索引、或会话变化超过 15 s 仍未写入时为 true，`progress.done` 在同一轮构建内不减少。

## 会话清理

`/v1/cleanup/*`、`/v1/archive` 与 `uniflo clean --json`、`uniflo archive --json` 的类型，真源 `crates/uniflo-schema/src/cleanup.rs`。接口与 `reason` 取值见 `docs/api.md#会话清理`。

| 类型 | 用途 | 要点 |
|---|---|---|
| `CleanupRequest` | `POST /v1/cleanup/plan` 请求体 | `sessions[]`（会话 key）、`q?`（会话搜索语法） |
| `CleanupPlan` | 计划 | `plan_id`、`created_at`、`expires_at`、`sessions[]`（`CleanupCandidate`，按请求顺序）、`freed_bytes`、`archive_bytes`（预计） |
| `CleanupCandidate` | 计划里的一个会话 | `key`、`harness`、`title?`、`eligible`、`reason?`、`message?`、`children[]`（随它一起清理的子代理）、`targets[]`、`bytes`、`archive_bytes`（预计） |
| `CleanupTarget` | 一个要移走的文件或目录 | `path`、`dir`、`bytes`、`files`、`mtime_ms`（目录取最新的文件）、`file_id?`（inode） |
| `CleanupReport` | 执行结果 | `plan_id`、`results[]`、`freed_bytes`、`archive_bytes`（实际） |
| `CleanupResult` | 一个会话的结果 | `key`、`status`（`archived` / `failed` / `skipped`）、`reason?`、`message?`、`freed_bytes`、`archive_bytes`、`children[]` |
| `ArchiveList` | `GET /v1/archive` | `archives[]`（`ArchiveEntry`）、`bytes` |
| `ArchiveEntry` | 一个归档会话 | `key`、`harness`、`id`、`title?`、`cwd?`、`root?`（随哪个会话一起清理）、`path`（归档文件）、`bytes`、`source`（原位置）、`source_bytes`、`archived_at`、`restored`（源文件已还原） |
| `ArchiveRemoved` | `DELETE /v1/archive/{key}` | `removed[]`、`bytes` |

归档文件 `<数据目录>/archive/<harness>/<id>.jsonl.zst`：zstd 压缩的 JSONL，第一行是 `Session`（`archived: true`），之后每行一个 `Event`（同一 id 只留最新版本）。精简规则：user / assistant / reasoning / system 正文每条最多 16 KiB，tool_result 输出最多 2 KiB，tool_call 参数序列化后超过 2 KiB 时换成截短的 JSON 文本（字符串），连续 ≥ 256 个 base64 字符（同时含大写、小写和数字）换成 `[binary omitted: N bytes]`，被改动的事件标 `truncated: true`；usage 事件原样保留，缺 `model` 时补上用量账本解析出的模型。

## 示例

```json
{"type":"event","seq":812,"event":{"id":"msg_01:2","session":"claude:4f1c…","ts":1790000000123,"pos":53311,"kind":"tool_call","call_id":"toolu_01","name":"Bash","input":{"command":"cargo test"}}}
{"type":"session","seq":813,"session":{"key":"claude:4f1c…","harness":"claude","id":"4f1c…","source":"/Users/me/.claude/projects/-x/4f1c….jsonl","updated_at":1790000000123,"status":"work","status_since":1790000000001,"status_reason":"tool_call","pid":55935}}
```
