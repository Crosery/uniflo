# Harness 适配

> 已支持的 harness、各自的存储与回合信号，跨平台路径映射（macOS、Linux、Windows），以及新增一个 harness 的步骤。

状态：`current` · 更新：2026-10-09

## 已支持

| id | 名称 | 存储（`~` = 家目录 / `%USERPROFILE%`） | 格式 | 回合结束信号 | 进程映射 |
|---|---|---|---|---|---|
| `claude` | Claude Code | `~/.claude/projects/<slug>/<id>.jsonl`，子代理 `<id>/subagents/**/agent-*.jsonl` | JSONL | `stop_reason` end_turn/stop_sequence、`turn_duration`、中断 | `~/.claude/sessions/<pid>.json` |
| `qoder` | Qoder | `~/.qoder{,-cn}/projects` | 同 Claude | 同 Claude | — |
| `qwen` | Qwen Work | `~/.qwenworkcn/projects` | 同 Claude | 同 Claude | — |
| `codex` | Codex | `~/.codex/{sessions,archived_sessions}/**/rollout-*.jsonl` | JSONL | `task_complete` / `turn_aborted` | —（app-server 一进程多会话） |
| `pi` | Pi | `~/.pi/agent/sessions/<slug>/<ts>_<id>.jsonl` | JSONL | `stopReason` | `ps` + `lsof`（Win: PowerShell） |
| `omp` | oh-my-pi | `~/.omp/agent/sessions/…` | 同 Pi | 同 Pi | `ps` + `lsof`（Win: PowerShell） |
| `crosery` | Crosery Agent | `~/.crosery/agent-sessions` | 同 Pi | 同 Pi | — |
| `commandcode` | Command Code | `~/.commandcode/projects` | 同 Pi（Anthropic 块） | 纯文本回复 | — |
| `prime` | Prime Agent | `~/.prime/agent/sessions/<uuid>.jsonl`（根会话），子代理 `~/.prime/agent/session-artifacts/<uuid>/**/<sub-uuid>.jsonl` | JSONL（扁平根 + 递归工件） | `stopReason` | `~/.prime/agent/daemon-workers/<id>/*.json` + `ps` |
| `cline` | Cline | `~/Library/Application Support/...`（Linux: `~/.config/...`，Windows: `%APPDATA%\...\User\globalStorage\saoudrizwan.claude-dev\tasks\<id>`） | 完整 JSON 数组 | `completion_result` / `ask_followup` / `end_turn` | — |
| `roo` | Roo Code | 同 Cline 路径结构，覆盖 `rooveterinaryinc.roo-cline`, `roovscode.roo-cline`, `kilocode.kilo-code` | 同 Cline | 同 Cline | — |
| `kodu` | Kodu | 同 Cline 路径结构，`kodu-ai.kodu` | 同 Cline | 同 Cline | — |
| `gemini` | Gemini CLI | `~/.gemini/tmp/<project>/chats/session-*.jsonl`（旧版 `.json`） | JSONL，消息原地重写 | 无工具调用的回复 | — |
| `antigravity` | Antigravity | `~/.gemini/antigravity{,-cli}/brain/<id>/…/transcript.jsonl` | 步骤 JSONL | 无工具调用的规划步骤 `DONE` | — |
| `opencode` | OpenCode | `~/.local/share/opencode/opencode.db`（Windows: `%LOCALAPPDATA%\opencode\opencode.db`） | SQLite | 助手消息完成且 `finish != tool-calls` | `ps` + cwd 查询（Win: PowerShell） |
| `kilo` | Kilo Code | `~/.local/share/kilo/kilo.db`（Windows: `%LOCALAPPDATA%\kilo\kilo.db`） | 同 OpenCode | 同 OpenCode | `ps` + cwd 查询（Win: PowerShell） |
| `zcode` | ZCode | `~/.zcode/cli/db/db.sqlite` | 同 OpenCode | 同 OpenCode | `ps` + cwd 查询（Win: PowerShell） |
| `mimocode` | MiMo Code | `~/.local/share/mimocode/mimocode.db`（Windows: `%LOCALAPPDATA%\mimocode\mimocode.db`） | 同 OpenCode | 同 OpenCode | `ps` + cwd 查询（Win: PowerShell） |
| `workbuddy` | WorkBuddy | `~/.workbuddy/projects/<slug>/<id>.jsonl` | JSONL | 助手消息完成 | `~/.workbuddy/sessions/<pid>.json` |
| `minimax` | MiniMax Code | `~/.minimax/v2/sqlite/runtime-state.sqlite` | SQLite（WAL，助手行原地重写） | `turn_ingress` completed/failed/aborted | `turn_ingress` accepted + `ps`/`lsof`（Win: PowerShell） |
| `hermes` | Hermes | `~/.hermes/state.db` | SQLite | 终止型 `finish_reason` | — |
| `factory` | Factory Droid | `~/.factory/sessions/<slug>/<id>.jsonl` | JSONL | 纯文本回复 | — |
| `reasonix` | Reasonix | `~/.reasonix/projects/<slug>/sessions/*.events.jsonl` | 追加 / 替换日志 | 纯文本回复 | — |
| `cursor` | Cursor Agent | `~/.cursor/projects/**/agent-transcripts/<id>/<id>.jsonl` | JSONL（无 id、无时间） | 纯文本回复 | — |
| `dsh` | DeepSeek Harness | `~/.dsh/sessions/<cwd-slug>/<id>/session[.v4].jsonl.zstd` | zstd 帧批量 JSONL（帧 = 一次 flush 的若干整行；行永不跨帧） | `turn/end` reason completed/aborted/interrupted/error | `session.lock` 被 harness 进程持有 → `lsof`（Win 无 lsof，退化为事件规则） |

## usage 口径

`usage` 事件统一为：`input` 不含缓存、`output` 含思考、`reasoning ⊂ output`（口径表见 `docs/schema.md#usage-口径`）。各家原始字段的换算（2026-10-09 按本机真实数据核对计数）：

| harness | 来源 | 换算 | `model` | `cost_usd` |
|---|---|---|---|---|
| claude / qoder / qwen | `message.usage`（Anthropic 形） | 原样：`input_tokens` 本就不含缓存，`output_tokens` 含思考 | `message.model`；`<synthetic>` 模型的占位用量跳过 | — |
| codex | `token_usage_record.usage`（权威，id `<response_id>:usage`）；没有时 `token_count.info.last_token_usage` | `input = input_tokens − cached_input_tokens − cache_write_input_tokens`；`output_tokens` 已含 `reasoning_output_tokens` | 最近的 `turn_context.model` | — |
| pi / omp / crosery / commandcode | 助手消息 `usage` | 原样；`reasoning > output` 的行（output 未含思考）把思考加回 output | `message.model` | `usage.cost.total` |
| prime | 同 Pi | 同 Pi | `message.model` | `usage.cost.total` |
| cline / roo / kodu | `api_req_started` | 原样（Cline 已把缓存从 `tokensIn` 拆出） | 记录里的 `model` | `cost` |
| gemini | 消息 `tokens` | `input = input + tool − cached`；`output = output + thoughts`；`reasoning = thoughts` | 消息 `model` | — |
| opencode / kilo / zcode / mimocode | `step-finish` part 的 `tokens` | `total = i+o+r+c` 的版本 output 不含思考 → 加回；`total = i+o+c` 的版本已含 → 原样；无 `total` 时同 Pi 的启发式 | 消息 `modelID` | part 的 `cost` |
| workbuddy | 助手消息与工具调用记录的 `providerData.usage`（OpenAI completions 形） | `input = inputTokens − Σ inputTokensDetails[].cached_tokens`，后者记为 `cache_read`；`reasoning = Σ outputTokensDetails[].reasoning_tokens` | `providerData.model` | —（`credit` 不是美元） |
| minimax | 助手行 `usage` | 原样（`input_tokens` 不含缓存） | `context_usage_telemetry.model` | — |
| dsh | `assistant/message` 的 `usage` | 原样 | 无；由引擎用 `request/context` 元数据里的模型补 | — |
| hermes | `sessions` 表的 token 与成本列 | 每会话一条会话级 usage（id `session:usage`，无 `pos`），随会话行重发 | `sessions.model` | `actual_cost_usd`，否则 `estimated_cost_usd` |
| antigravity / cursor / factory / reasonix | 不记录用量 | — | — | — |

- 重复计数：Codex 一旦出现 `token_usage_record`，同文件的 `token_count` 都是它的重复，全部跳过；旧 rollout 只有 `token_count`，`total_token_usage` 未变化的重复行（限流刷新）跳过。Pi / Prime 的 `child_usage_attributed` 是子代理文件自身用量的合计（真实数据逐 token 相等），子代理会话本身已计，故不再产出 usage。
- `cost_usd` 只在 harness 自报正数时填写；≤ 0 视为未报，由价格目录计算。
- 新适配器：先确认原始 input 是否含缓存、output 是否含思考（看 `total` 字段或 `cache_read > input` 这类不可能关系），再按上表口径换算，并在测试里断言换算后的四项。

## 跨平台路径解析规约

1. **家目录与用户配置**：统一通过 `uniflo_core::util::home()` 获取。在 macOS/Linux 上解析为 `$HOME`，在 Windows 上解析为 `%USERPROFILE%`（如 `C:\Users\Username`）。
2. **应用全局漫游存储（VS Code / Cursor / Windsurf 等扩展）**：
   - macOS: `~/Library/Application Support/<app>/User/globalStorage`
   - Linux: `~/.config/<app>/User/globalStorage`
   - Windows: `%APPDATA%\<app>\User\globalStorage`（如 `C:\Users\Username\AppData\Roaming\<app>\User\globalStorage`）
3. **本地应用数据与 SQLite 数据库（Kilo / OpenCode / MiMoCode）**：
   - macOS / Linux: `~/.local/share/<app>`
   - Windows: OpenCode 系 CLI 在 Windows 上仍沿用 XDG 布局 `~\.local\share\<app>`（实测），因此两个候选都探测、优先实际存在数据库的路径：`~\.local\share\<app>` 与 `%LOCALAPPDATA%\<app>`。
4. **进程与活体探测**：
   - Unix (macOS / Linux): `libc::kill(pid, 0)` 信号探测，配合 `ps` 与 `/proc` 或 `lsof`。
   - Windows: Win32 原生 API `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess`（状态码 259 `STILL_ACTIVE`），配合 PowerShell `Get-CimInstance Win32_Process`；工作目录经 `NtQueryInformationProcess` + `ReadProcessMemory` 读 PEB（x64，失败返回 `None`）；没有 `lsof` 等价物，打开文件映射退化为 argv 启发式。

每个模块文件头有格式细节与怪癖（`crates/uniflo-adapters/src/<模块>.rs`）。没有回合结束标记的 harness 依赖超时规则（`docs/architecture.md#状态机`）。

## 新增一个 harness

1. **调研格式**：只看结构不看内容。统计记录类型、字段名、文件布局，输出计数；不要把真实会话正文贴进对话、注释或测试。
2. **选接口**：
   - 一个文件 = 一个会话的 JSONL / JSON → 实现 `uniflo_core::LineDecoder`，用 `JsonlAdapter::new(...)` 包装，自动获得头尾摘要、跟随、分页、去重。
   - 数据库或一文件多会话 → 实现 `uniflo_core::Adapter`；SQLite 用 `crate::sqlite::open_ro`，跟随按 rowid，绝不全表扫描。
3. **映射**：每条原始记录 → `cx.emit(id, ts, Body)`。`id` 要在会话内稳定，同一条消息被原地更新时复用同一 id。工具调用与结果用同一个 `call_id` 配对。有显式回合边界时发 `TurnStart` / `TurnEnd`；不要在适配器里推断状态。
4. **元数据**：`cx.meta()` 填 `cwd`、`model`、`started_at`、`title`（带优先级：1 摘要 < 2 自动标题 < 3 用户命名）、`parent`。
5. **未知记录**：`cx.unknown("type=…")`；确认无用的类型加进模块的 `IGNORED`。
6. **注册**：`Cargo.toml` 加 feature 并放进 `default`；`lib.rs` 加 `#[cfg(feature = "…")] pub mod …;` 并在 `all()` 中注册。
7. **测试**：`common::testkit::Fixture` 写合成文件，至少断言一个完整回合的事件 kind 序列、最终状态、元数据、`unknown` 为空；有子代理/分叉时测 `identify()` 的 id 与 parent；有用量时断言换算后的 `input` / `output` / `cache_read` / `reasoning` / `model`。
8. **真实数据验收**：`cargo build --release && ./target/release/uniflo scan --no-cache --json`，看该 harness 的会话数，以及 `stats.unknown` 为空、`bad_lines`、`read_errors` 为 0。
9. **进程映射（可选）**：harness 有 pid 注册表时实现 `live()` + `live_roots()`；没有时可用 `uniflo_core::procs::ProcCache` 探测进程，并优先用确定性证据（命令行参数、打开的文件）。
