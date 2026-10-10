# harness-adapters 规格

Uniflo 通过 `uniflo-adapters` 中的适配器只读地读取各 harness 的会话存储，并归一成 wire schema 的 `Session` 与 `Event`。本规格描述新增八个 harness 的适配，以及 cursor 补读 IDE 元数据之后的完整行为。既有适配器的 usage 口径与金额由 usage-cost 规格负责。

## 通用要求

- 每个新 harness 都要做到以下几点：
  - 作为 `uniflo-adapters` 的一个 cargo feature 进入 `default`，并在 `all()` 里注册。
  - 单独执行 `cargo check --no-default-features --features <id>` 时没有警告。
  - 在 `docs/adapters.md` 补一行，写明 id、存储位置（macOS / Linux / Windows）、格式、回合结束信号、存活进程映射方式、恢复命令。
  - README 的 harness 列表同步更新。
- 存储路径支持各 harness 的环境变量覆盖（见下文）。覆盖后的目录必须真的含有会话目录才采信。
- 读取方式：
  - SQLite 一律走 `sqlite::open_ro`，只做短查询，不复制、不加锁。
  - JSONL 能追加跟随的就按偏移跟随。
  - 会被原地重写的源，用 `ReadOutput.reset` 整源重读。源文件暂时缺失时沿用上次结果，不报错。
- 输出要求：
  - usage 事件的 token 字段遵循 usage-cost 规格定义的口径，能取得模型名时填入步骤 `model`，harness 自带金额时填入 `cost_usd`。
  - 不认识的记录类型走 `cx.unknown(...)`；确认无用的类型加进 `IGNORED`。
- 测试夹具只用手写合成数据，至少断言以下几项：一个完整回合的事件 kind 序列、最终状态、元数据（标题、cwd、模型、时间）、`unknown` 为空。

## 新增 harness

| id | 存储 | 格式要点 | 回合结束 |
|---|---|---|---|
| `grok` | `~/.grok/sessions/<百分号编码的 cwd>/<uuid>/` | `updates.jsonl`：ACP 风格，消息与思考是流式片段，需合并且不能 trim；`tool_call` 加 `tool_call_update` 回填结果。`summary.json`：`generated_title` / `session_summary`、`current_model_id`、`info.cwd`、时间。`usage.json`：`turns[]`，每回合一条 usage。`events.jsonl`：`turn_started` / `turn_ended`。`subagents/*/meta.json` 给出父会话。没有任何对话内容的空壳会话不列出 | `_x.ai/session/update` 的 `turn_completed`（`stop_reason`），或 `events.jsonl` 的 `turn_ended`（`outcome`） |
| `kiro` | `~/.kiro/sessions/cli/<uuid>.jsonl` 加同名 `.json` 边车 | `Prompt` → user_message；`AssistantMessage` → assistant_message。cwd、标题、模型、时间取自边车 | 助手文本消息 |
| `kimi` | `$KIMI_CODE_HOME` 或 `~/.kimi-code` 下的 `sessions/wd_*/{session_,ses_}*/agents/main/wire.jsonl` | `turn.prompt` / `turn.steer`：当 `origin.kind` 不是用户时标为 `synthetic`。`context.append_loop_event` 的 `content.part`：`text` → assistant_message，`think` → reasoning。`tool.call` / `tool.result` 配对。`context.apply_compaction` → `system{compact}`。用户输入的回声行跳过。标题取自 `state.json`，"New Session" 是占位，忽略；`state.json` 写到一半时沿用上次读到的。cwd 取自 `session_index.jsonl`。`forkedFrom` 作为父会话 | `step.end` 之后没有新的 `step.begin`、且最后一步没有工具调用时，判为回合结束；有显式 turn 结束记录时以记录为准 |
| `copilot` | `$COPILOT_HOME` 或 `~/.copilot/session-store.db` | 只读 SQLite：`sessions` 与 `turns`。每一行 turn 依次产生 user_message、assistant_message、turn_end。按 `turns` 的 rowid 跟随 | 一行 turn 等于一个已结束的回合 |
| `openclaw` | `$OPENCLAW_STATE_DIR`、`~/.openclaw`，回落到 `~/.clawdbot`；路径为 `agents/<id>/agent/openclaw-agent.sqlite`，旧版为 `agents/<id>/sessions/*.jsonl` | 条目格式与 pi 的树形条目相同，复用 `pi.rs` 的解码。新库按 `transcript_events.seq` 跟随；可见分支以 `session_transcript_active_events` 为准，分支切换时整会话重读。`spawned_by` 作为父会话。同一会话既在库中又有旧 jsonl 时，以库为准 | pi 的 `stopReason` |
| `codebuddy` | `$CODEBUDDY_CONFIG_DIR` 或 `~/.codebuddy/projects/<slug>/<id>.jsonl`，子代理在 `<id>/subagents/agent-*.jsonl` | 与 WorkBuddy 同内核，复用其解码。补 `providerData.rawUsage`（OpenAI 形，prompt 已含缓存，需扣除）。标题优先级：custom > ai > topic，占位标题忽略 | `status=="completed"` 的助手 message |
| `craft` | `~/.craft-agent/workspaces/*/sessions/<id>/session.jsonl` | 首行是 SessionHeader，含 name、workingDirectory、model、tokenUsage、parentSessionId、createdAt / lastMessageAt；其余每行是一条 StoredMessage，工具调用一行内同时带 `toolInput` 和 `toolResult`。整文件重写，所以每次 reset 整源重读。id 取 `<工作区目录名>/<会话目录名>`。**不读工作区的 `config.json`**（含 token）。`{{SESSION_PATH}}` 占位符要展开 | 末条为助手文本 |
| `devin` | `$XDG_DATA_HOME/devin`、`~/.local/share/devin`、`~/Library/Application Support/devin`、`%APPDATA%\devin` 依次查找，取第一个存在的 `cli/sessions.db` | 只读 SQLite：`sessions` 与 `message_nodes`。可见链从 `main_chain_id` 沿 `parent_node_id` 回溯到根。`hidden=1` 的会话不列。`metadata.is_user_input==false`、内部 telemetry 来源、compaction 请求都标为 synthetic 或 system。`metadata.metrics` 转成 usage，`generation_model` 作为步骤模型。主链变化时整会话重读 | 没有 `tool_calls` 的助手消息 |

## cursor 补读 IDE 元数据

- 只读打开 Cursor IDE 的 `state.vscdb`，路径为 `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`；Linux 用 `~/.config/Cursor/...`，Windows 用 `%APPDATA%\Cursor\...`。
- 用 `composerData:<id>` 和 `composerHeaders` 为 `agent-transcripts` 中 id 相同的会话补上标题、cwd、模型、创建时间、更新时间和子代理的父会话。
- IDE 库里有气泡正文的 composer（旧版 Cursor），按 `fullConversationHeadersOnly` 的顺序产生消息事件；`tokenCount` 转成 usage。
- 没有任何正文的 composer 只用于补元数据，不单独列成会话。
- 枚举时不对全部 blob 执行 `json_extract`，只按键前缀查询，并且只在库的 mtime 或 WAL 变化时重读。

### Scenario: grok 合成会话完整回合
- GIVEN 一个合成的 grok 会话目录，内含 `updates.jsonl`（用户片段、思考片段、助手片段、tool_call、tool_call_update、turn_completed）、`summary.json`、`usage.json`、一个子代理目录
- WHEN 用 grok 适配器索引
- THEN 事件序列依次为 user_message、reasoning、tool_call、tool_result、assistant_message、usage、turn_end
- AND 片段合并后的文本与原片段逐字拼接一致，没有被 trim
- AND 会话的标题、cwd、模型、时间来自 `summary.json`
- AND 子代理会话的 `parent` 指向父会话
- AND 最终状态为 idle，`unknown` 为空
- AND 只有 hook 行的空壳会话不列出

### Scenario: grok 本机真实数据计数验收
- GIVEN 本机 `~/.grok/sessions`
- WHEN 构建 release，运行 `uniflo scan --no-cache --json`
- THEN `grok` 的会话数等于该目录下含对话内容的会话目录数
- AND `stats.unknown` 中没有 grok 条目，`bad_lines` 与 `read_errors` 为 0
- AND 过程中只输出计数与键名

### Scenario: kiro 合成会话
- GIVEN 一个合成的 kiro `.jsonl`（Prompt 加 AssistantMessage）及其 `.json` 边车
- WHEN 索引
- THEN 依次产生 user_message 和 assistant_message
- AND 标题、cwd、模型、时间来自边车
- AND 最终状态为 idle

### Scenario: kimi 合成会话
- GIVEN 一个合成的 kimi 会话：`wire.jsonl` 含用户 prompt、用户输入的回声行、think、text、tool.call、tool.result、step.end、compaction、一个 origin 为 system_trigger 的 prompt；同时有 `state.json`（标题为 "New Session"）和 `session_index.jsonl`
- WHEN 索引
- THEN 回声行不产生重复的 user_message
- AND system_trigger 的输入被标为 `synthetic`
- AND compaction 产生 `system{compact}`
- AND 标题回退到首条用户输入，不显示 "New Session"
- AND cwd 来自 `session_index.jsonl`
- AND `state.json` 被截断成空文件时，会话标题保持上次的值

### Scenario: copilot 合成库
- GIVEN 一个合成的 `session-store.db`，含两个会话、三行 turns
- WHEN 索引，然后再追加一行 turn
- THEN 每行 turn 依次产生 user_message、assistant_message、turn_end
- AND 追加的那一行按 rowid 增量读入，不重读已有行
- AND 数据库始终只读打开

### Scenario: openclaw 新库与旧 jsonl
- GIVEN 一个合成的 `openclaw-agent.sqlite`（`transcript_events` 含 pi 树形条目，`session_transcript_active_events` 指定可见分支）和一个同 id 的旧版 jsonl
- WHEN 索引
- THEN 只列出一个会话，内容来自库中的可见分支
- AND usage 带 `cost_usd`（取 pi 格式中的 `usage.cost.total`）
- AND `spawned_by` 非空的会话有 `parent`

### Scenario: codebuddy 合成会话
- GIVEN 一个合成的 codebuddy jsonl：含 message、reasoning、function_call、function_call_result、`custom-title`、`ai-title`，以及 `providerData.rawUsage`
- WHEN 索引
- THEN 标题取 custom-title
- AND usage 的 `input` 等于 prompt_tokens 减去缓存命中数
- AND 子代理文件挂到父会话下
- AND 遇到 `status=completed` 的助手 message 后状态为 idle

### Scenario: craft 整文件重写
- GIVEN 一个合成的 craft `session.jsonl`
- WHEN 用"写临时文件、删除正本、重命名"的方式把它整体重写为追加了一条消息的新版本，期间正本短暂不存在
- THEN 索引结果包含新消息且没有重复
- AND 正本不存在的那段时间里会话不消失，也不产生 read_errors
- AND 不读取工作区 `config.json`

### Scenario: devin 主链与隐藏会话
- GIVEN 一个合成的 devin `sessions.db`：一个会话带有重试侧枝和 `main_chain_id`，另一个会话 `hidden=1`
- WHEN 索引，然后把 `main_chain_id` 改为指向侧枝
- THEN 只列出非隐藏会话
- AND 事件来自主链，侧枝上的消息不出现
- AND `metadata.metrics` 转成 usage，`generation_model` 写入步骤 model
- AND 主链变更后，会话被整体重读，内容与新主链一致

### Scenario: cursor IDE 元数据补全
- GIVEN 一个合成的 `agent-transcripts/<id>.jsonl`，以及合成的 `state.vscdb`：含同 id 的 `composerData`（带模型、时间、workspace 路径）和 `composerHeaders`，另有一个带气泡正文的 IDE-only composer、一个没有任何正文的 composer
- WHEN 索引
- THEN 该 transcript 会话获得标题、cwd、模型、开始时间与更新时间
- AND 带气泡的 IDE-only composer 作为 cursor 会话列出，消息顺序与 `fullConversationHeadersOnly` 一致
- AND 没有正文的 composer 不单独列出
- AND 在本机真实 `state.vscdb` 上运行 scan 时只读打开，不产生复制或锁文件，并输出被补全的会话计数

### Scenario: 新 feature 独立编译与文档
- GIVEN 新增的八个 feature
- WHEN 运行 `scripts/verify.sh`
- THEN 每个 feature 单独 `cargo check` 无警告
- AND `docs/adapters.md` 的表格与 README 的 harness 列表包含全部新 id
- AND 对没有本机数据的 harness，文档中标注"未经真机验证"
