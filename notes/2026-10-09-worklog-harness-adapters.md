# 2026-10-09 worklog · harness-adapters

> service-expansion 子任务 harness-adapters：新增 grok、kiro、kimi、copilot、openclaw、codebuddy、craft、devin 八个适配器，cursor 补读 IDE `state.vscdb`；真实数据只看计数、键名与类型。

状态：`historical` · 更新：2026-10-09

## 21:45 · 八个新 harness 与 Cursor IDE 元数据

### 完成条件

- `docs/comet/changes/service-expansion/specs/harness-adapters/spec.md` 的 11 个 Scenario 都有自动化或真实数据证据。
- release 构建跑 `uniflo scan --no-cache --json`：`grok` 会话数等于 `~/.grok/sessions` 下含对话内容的会话目录数；全部 harness 的 `stats.unknown` 为空，`bad_lines`、`read_errors` 为 0；既有 25 个 harness 的会话数与改动前的二进制同时刻扫描结果逐个相等。
- 8 个新 feature 单独 `cargo check --all-targets` 无警告（`RUSTFLAGS="-D warnings"`）；`scripts/verify.sh` 通过。
- Cursor 的 `state.vscdb` 只读打开：扫描前后 globalStorage 目录文件数不变，库文件与 `-wal` 的大小、mtime 不变。

### 改动与依据

- core（`crates/uniflo-core/src/jsonl.rs`）：
  - `LineDecoder::finish`：一次连续读取窗口结束时调用，把未闭合的流式片段以 `partial` 发出，完整版复用同一 id（只在本次确实读到新行时调用，空跟随不重发）。
  - `decode_record`：把文件以外取得的 JSON 值交给某个 `LineDecoder` 解码（OpenClaw 库内条目复用 Pi 解码）。
- adapters 公共件（`crates/uniflo-adapters/src/common.rs`）：`Sidecar` + `WithSidecars` 包装器读会话目录里原地重写的旁路 JSON；只在摘要 / 重置、会话首次有内容、旁路签名变化时补发；只有元数据没有事件的会话不列出（不让空壳出现在列表里）。
- 新模块：`grok.rs`、`kiro.rs`、`kimi.rs`、`copilot.rs`、`openclaw.rs`、`codebuddy.rs`、`craft.rs`、`devin.rs`；`cursor_ide.rs`（IDE 库）+ `cursor.rs`（transcript 与 IDE 库合成一个 `cursor` 适配器）。
  - grok：片段合并不 trim；`turn_completed` 或 `events.jsonl` 的 `turn_ended` 结束回合；`usage.json` 用 `totalTokens` 判断缓存 / 思考是否已计入再换算。
  - kimi：`turn.prompt` 的 `origin` 决定 `synthetic`；`context.append_message` 的用户回声跳过；`state.json` 截断时沿用上次成功解析的标题。
  - copilot：`turns` 每行 → user / assistant / turn_end，按 rowid 跟随。
  - devin / openclaw：消息树只展示可见分支，主链切换时整库重读（`reset`）；openclaw 新库优先于同 id 的旧 jsonl，`usage.cost.total` 进 `cost_usd`。
  - codebuddy：复用 WorkBuddy 内核（`WorkBuddy::new(info, base)`），新增 `providerData.rawUsage` 换算、`custom-title` / `ai-title` / `topic` 标题优先级、`role=system`。WorkBuddy 自身的模型优先级保持 `providerData.model` → `requestModelName`（本机数据里两者逐条不同，前者才是实际模型）。
  - craft：源是会话目录，每次读取整源重读并 `reset`；正本短暂缺失时保留上次结果；从不读工作区 `config.json`；头部 `tokenUsage` 已由 `claude` 逐步计入，不再产出 usage。
  - cursor IDE：只按键前缀区间枚举 `bubbleId:` / `composerData:`、按主键取单值，不对 blob 做 `json_extract`；按库文件 + WAL 签名缓存。
- 环境变量覆盖：`KIMI_CODE_HOME`、`COPILOT_HOME`、`OPENCLAW_STATE_DIR`、`CODEBUDDY_CONFIG_DIR`、`XDG_DATA_HOME` / `APPDATA`（devin），目录确有会话数据才采用。
- 依赖：没有新增生产依赖；`uniflo-adapters` 的 dev-dependencies 加 workspace 已有的 `tokio`（craft 的引擎级重写测试），`Cargo.lock` 仅多这一条边。
- 文档：`docs/adapters.md`（表格加「恢复命令」列，按 agent-access 规格的表填写；8 行新 harness；cursor 行补 IDE 库三平台路径；usage 口径表；未经真机验证说明）、`README.md`（25 → 33）、`docs/architecture.md`（读取模型与存活探测）。

### 验证命令与真实结果

- `UNIFLO_HOME=/tmp/uniflo-ha-verify-home UNIFLO_TEST_PORT=7421 scripts/verify.sh` → `verify: all checks passed`（exit 0）：rustfmt、clippy `-D warnings`、24 个 feature 各自单独编译（含 grok、kiro、kimi、codebuddy、copilot、devin、craft、openclaw）、分支不变量（仅有 `comet/supervisor/...` 分支命名警告）、`cargo test --workspace` 191 通过 0 失败、release 守护进程冒烟（`/v1/harnesses` 33 个）。未加 `--e2e`：没动网关与网页演示。
- 真实数据扫描（release，`--no-cache --json`，只取计数）。改动前二进制（基线提交 `3cd9c60` 的 `git archive` 构建）与新二进制背靠背各跑一次：

  | 指标 | 基线 | 新 |
  |---|---|---|
  | harness 数 | 25 | 33 |
  | 会话数 | 6567 | 6568（+1 = grok） |
  | 源文件数 | 5827 | 5831 |
  | `stats.unknown` | `{}` | `{}` |
  | `bad_lines` / `read_errors` | 0 / 0 | 0 / 0 |

  - 既有 25 个 harness 的会话数逐个相等；新 harness：grok 1，其余 7 个为 0（本机无数据）。
- grok 计数核对：`~/.grok/sessions` 下 2 个会话目录，各有 `updates.jsonl`。按记录类型计数：一个只有 `hook_execution`（`chat_history.jsonl` 只有 system 与 synthetic user），另一个有 `user_message_chunk`、`retry_state`、`turn_completed`。含对话内容的会话目录 = 1 = scan 的 `grok` 会话数。该会话事件 kind：user_message 1、system 1、turn_end 1，状态 idle，标题 / cwd / 模型 / 开始时间都有值；`usage.json` 唯一回合的 token 全为 0，所以没有 usage 步骤（`/v1/sessions/{key}/usage` steps 0）。
- Cursor IDE（`HOME=<tmp> UNIFLO_HOME=$HOME UNIFLO_DATA_DIR=<tmp>`，守护进程绑 `127.0.0.1:7422/7424`，`--no-cache`）：cursor 9 个会话全部来自 transcript；被 IDE 元数据补全的会话数 0（`composerData` 106 行、`composerHeaders` 90 行、`bubbleId` 0 行；9 个 transcript id 与 composer id 无一重合），所以 IDE-only 会话也是 0。
- `state.vscdb` 只读核对（scan 前后 `stat`）：globalStorage 目录 6 个文件不变；`state.vscdb` 2826240 B、mtime 不变；`state.vscdb-wal` 0 B、mtime 不变；`state.vscdb-shm` 大小不变，mtime 更新（见残余风险）。
- craft 的 key 含 `/`：`UNIFLO_HOME=<tmp>` 下合成一个 craft 会话，守护进程绑 `127.0.0.1:7423`；`/v1/sessions/craft%3Aws1%2F260901-brave-otter` 200，事件 user_message、assistant_message、turn_end；未编码的 `/` 404（CLI `enc()` 与网页 `encodeURIComponent` 都会编码）。
- 测试守护进程全部停止；`127.0.0.1:7311` 上的常驻守护进程未触碰；`~/Library/Application Support/uniflo` 无写入。

### Scenario → 证据

| Scenario | 证据 | 结论 |
|---|---|---|
| grok 合成会话完整回合 | `grok::tests::full_turn_merges_fragments_and_reads_siblings`、`subagent_session_points_at_parent_and_hook_only_shell_is_unlisted`、`streaming_follow_flushes_partial_then_completes`、`late_usage_and_events_turn_end_arrive_through_siblings`、`usage_normalization_follows_total` | PASS |
| grok 本机真实数据计数验收 | 上文 scan 与目录计数：1 = 1，unknown 空，0 / 0 | PASS |
| kiro 合成会话 | `kiro::tests::prompt_and_reply_with_sidecar_metadata`、`prompt_only_is_working_and_sidecar_change_is_a_change` | PASS（未经真机验证） |
| kimi 合成会话 | `kimi::tests::full_turn_echo_synthetic_compaction_and_index_cwd`、`idle_after_tool_free_step_and_title_survives_truncated_state`、`metadata_only_wire_is_not_listed`、`migrated_history_without_steps` | PASS（对话映射未经真机验证） |
| copilot 合成库 | `copilot::tests::turns_become_exchanges_and_follow_by_rowid`（两个会话三行 turns，另一个无 turn 的会话不列出；追加一行只读到该行；`open_ro` 写入失败、目录文件列表不变） | PASS（未经真机验证） |
| openclaw 新库与旧 jsonl | `openclaw::tests::store_branch_wins_over_legacy_copy_with_cost_and_parent`、`legacy_jsonl_with_index_metadata` | PASS（未经真机验证） |
| codebuddy 合成会话 | `codebuddy::tests::turn_titles_raw_usage_and_subagent`、`placeholder_ai_title_falls_back_to_topic` | PASS（未经真机验证） |
| craft 整文件重写 | `craft::tests::whole_file_rewrite_keeps_session_and_reads_new_version`（引擎级：临时文件 → 删除 → 改名，会话不消失、`read_errors` 0、id 不重复、`config.json` 设为 000 也不影响）、`rewrite_read_is_a_reset_and_missing_file_keeps_cursor`、`header_messages_and_tool_rows` | PASS（未经真机验证） |
| devin 主链与隐藏会话 | `devin::tests::main_chain_hidden_sessions_usage_and_branch_switch`、`old_schema_without_leaf_shows_every_node` | PASS（未经真机验证） |
| cursor IDE 元数据补全 | `cursor::tests::ide_store_fills_transcript_metadata_and_lists_ide_only_composers`、`cursor_ide::tests::skip_scan_finds_each_composer_with_bubbles_once`；真实库只读核对与补全计数 0 | PASS（合成）；真实补全 0 条，本机无可匹配数据 |
| 新 feature 独立编译与文档 | `scripts/verify.sh` 各 feature 单独编译；`docs/adapters.md` 表格与 README 列表含 8 个新 id；未经真机验证已标注 | PASS |

### 残余风险

- `state.vscdb-shm` 的 mtime 在扫描后更新：WAL 模式下只读连接也会在共享内存索引里写读标记。这是 `sqlite::open_ro` 的既有行为（opencode、hermes、minimax 同样），没有新文件，库文件与 `-wal` 不变。
- kiro、copilot、openclaw、codebuddy、craft、devin 本机没有数据，kimi 只有空壳，cursor IDE 补全本机无匹配：格式依据公开源码与调研结论，只由合成夹具覆盖，真实版本若改字段会走 `unknown` 或缺元数据。
- codebuddy 的存活进程映射沿用 WorkBuddy 的 `<base>/sessions/<pid>.json`，未经真机验证。
- craft 的 id 含 `/`，第三方客户端若不对 key 做百分号编码会 404。
