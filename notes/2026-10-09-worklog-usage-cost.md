# 2026-10-09 worklog · usage-cost

> service-expansion 子任务 usage-cost：统一 usage 口径、模型价格目录与同步、按步计费、会话合计、`/v1/usage` 等四个接口与 `uniflo usage` / `uniflo pricing`；真实数据只看计数与金额。

状态：`historical` · 更新：2026-10-09

## 19:20 · 用量与费用

### 完成条件

- `docs/comet/changes/service-expansion/specs/usage-cost/spec.md` 的 10 个 Scenario 全部有自动化或真实数据证据。
- 真实数据上 `GET /v1/usage?group_by=model` 与 `uniflo usage --by model --json` 各模型 token、费用逐项相同；未定价模型单独计 `unpriced_steps`，不按 0 计。
- 守护进程启动与 `/v1/health` 不等用量全量读取（后台构建 + 持久缓存）；有缓存时启动到 `/v1/health` 的耗时比改动前多不超过 50%（release，`--bind 127.0.0.1:74xx`）。
- `scripts/verify.sh --e2e` 通过；`uniflo scan --no-cache --json` 的 `stats.unknown` 为空，`bad_lines`、`read_errors` 为 0。

### 改动与依据

- schema（只增）：`Usage.model`、`Usage.cost_usd`、`Session.usage`（`SessionUsage`，装箱避免大枚举变体）及 `UsageReport` / `SessionUsageDetail` / `ModelInfo` / `PricingStatus` 等响应类型（`crates/uniflo-schema/src/usage.rs`）；`docs/schema.md#usage-口径` 同步。
- 适配器口径审计（`docs/adapters.md#usage-口径` 逐个 harness 列出）：`input` 不含缓存、`output` 含思考、`reasoning ⊂ output`。
  - Codex：`token_usage_record` 优先；同一 rollout 出现过 record 后跳过 `token_count`，没有 record 的旧 rollout 跳过与上次累计相同的重复 `token_count`。
  - Gemini：`input + tool − cached`，`output + thoughts`；OpenCode 按 `total` 判断 output 是否已含 reasoning；WorkBuddy 从 prompt 中扣 `cached_tokens`；Hermes 用 sessions 表的 token / 金额列生成一条会话级 usage；Cline、OpenCode、Pi、Prime 读自报金额（≤0 视为未报）。
  - DSH：`read()` 的头尾抽样会丢中间的模型信息，新增 `read_all` 全量解码，供用量构建使用。
- 价格（`crates/uniflo-core/src/pricing/`）：三层目录 `overrides.json` > `catalog.json` > 内置快照（`scripts/build-pricing.mjs` 生成，104 KB）；模型名归一与近似匹配；系统 `curl` 同步，带分段、50% 安全阀、主源失败保留、原子写、24 h stale。决策写入 ADR-0010。
- 用量（`crates/uniflo-core/src/usage/`、`engine_usage.rs`）：按来源文件分账（`SourceLedger`），后台线程恢复 `usage-v1.json` → follow → 全量构建；价格文件变化即重新计价；`Session.usage` 随 session envelope 推送。
  - 同一会话 key 被多个来源持有（Claude 的 `subagents/` 与 `subagents/workflows/` 下同名 agent id）时用 `Ledger::combine` 合并，结果与读取顺序无关。
- 网关 `crates/uniflo-gateway/src/usage.rs`：`/v1/usage`、`/v1/sessions/{key}/usage`、`/v1/models`、`/v1/pricing`；`UsageParams::report` 同时供 CLI 本地模式使用。
- CLI `crates/uniflo-cli/src/usage.rs`：`uniflo usage`（表格 / `--json`，单个会话 key 给逐步明细）、`uniflo pricing [sync]`；daemon 增加 `--no-price-sync`。`--json` 原样输出守护进程的响应字节，避免 serde_json 默认浮点解析改动末位。
- `scripts/daemon-smoke.mjs`、`scripts/demo-e2e.mjs` 的守护进程加 `--no-price-sync`，测试不联网。
- 文档：`docs/api.md`、`docs/architecture.md`、`docs/adapters.md`、`docs/search.md`、`docs/conventions/DEVELOPMENT.md`、`README.md`、`AGENTS.md`、ADR-0010。

### 失败与教训

- 起初假设 Codex 的 `token_usage_record` 紧挨着对应的 `token_count`，实测下一行通常是 `function_call_output`，改为按 rollout 记录"是否出现过 record"。本机探测：有 record 的文件中每条 `token_count` 都与某条 record 成对或是重复。
- 真实数据上 Claude 有 8 组同名 agent id（不同目录、不同内容），按会话 key 单账本时总额随读取顺序变化；改为按来源分账、查询时合并后，本地模式与守护进程结果一致（如两个碰撞会话 272 = 99 + 173、118 = 89 + 29 步）。
- CLI 先解码再编码会让少数金额末位与 REST 不同；改为原样转发响应。未开 serde_json `float_roundtrip`（不在预批准依赖范围内）。
- DSH 抽样读取导致 6100 步没有模型；改用 `read_all` 后归零。
- 网关测试里引擎的 FS watcher 在临时目录布局下没有触发（既有行为，未在本任务改动）；实时推送测试改用工作中的"热"会话（50 ms 轮询）。

### 真实数据证据

隔离方式：`HOME=/tmp/uc-home-* UNIFLO_HOME=<真实家目录> UNIFLO_DATA_DIR=/tmp/uc-data-*`，守护进程只绑 `127.0.0.1:7401` / `7402`，`--no-update-check --no-price-sync`；只读 harness 数据，只打印计数、键名与金额。用户的 7311 守护进程未触碰。

- `uniflo scan --no-cache --json`（release 最终构建）：sessions 6557、files 5817、`unknown {}`、`bad_lines 0`、`read_errors 0`。
- 内置快照计价，全部来源：steps 380871、cost $33886.54、unpriced_steps 55752；分组行之和等于 totals（费用差 0）。
- CLI 与 REST（`/tmp/uc-scripts/final.py`，同一守护进程）：
  - `--by harness`：22 行，cli==rest True。
  - `--by model`：103 行，cli==rest True。
  - 步数最多的会话 `uniflo usage <key> --json` 对 `/v1/sessions/{key}/usage`：7641 步、125 回合，cli==rest True。
- 按 harness（费用前几位）：claude 154544 步 $15032.12、codex 59993 步 $7054.20、dsh 39868 步 $6245.77、omp 103511 步 $4667.95、pi 12783 步 $772.88。
- `/v1/models`：99 个模型，exact 53、approx 1（gemini-3.1-pro → gemini-2.5-pro）、none 45。
- 未定价步数最多的模型：qcn-qwen3.8-flash 21695、cline-deepseek-v4.1-flash 14037、deepseek-v4.1-flash 13678、codex-auto-review 1167、mimo-v2.6-pro 963、muse-spark-1.3-contributor 926、hy4-preview 426、gemini-3.8-flash-n 413；另有 101 步来自没有 `turn_context` 的旧 Codex rollout，模型为空。
- `/v1/usage` 延迟（全部会话）：harness 0.04 s、model 0.05 s、project 0.13 s、day 0.07 s、hour 0.06 s、weekday 0.05 s、cwd 0.05 s、session 0.05 s；`/v1/sessions/{key}/usage`（7641 步）0.01 s。
- 本地模式 `uniflo usage --local`：冷启动 11 s，有缓存 1.1 s。

启动耗时（release，交替测量，改动前构建 = base `fbc78d4`）：

| 场景 | 改动前 | 改动后 |
|---|---|---|
| 有缓存启动到 `/v1/health`（中位数） | 809 ms | 855 ms（+5.7%） |
| 更早一轮有缓存（三次） | 694 / 722 / 726 ms | 682 / 715 / 730 ms |
| 有缓存时用量 `ready` | — | 0.9–1.1 s |
| 无用量缓存：`/v1/health` / 用量 `ready` | — | 1828 ms / 12.0 s |
| RSS（有缓存，空闲） | 62–77 MB | 201–219 MB |

- 用量缓存 `usage-v1.json` 36 MB；冷构建峰值 RSS 491 MB，完整查询后约 306 MB。

价格同步实测（隔离数据目录 `/tmp/uc-data-sync`）：`uniflo pricing sync --json` → models.dev 成功，366 个模型，写出 `catalog.json` 与 `sync-state.json`；LiteLLM 失败 `curl: (35) Recv failure: Connection reset by peer`，记在 `error`；`uniflo pricing` 显示 source `models.dev`、stale false。

### 测试与检查

- 新增 / 更新测试：
  - core：`usage/mod.rs` 4 个、`report.rs` 3 个、`store.rs` 1 个、`tz.rs` 4 个；`pricing/catalog.rs` 5 个、`pricing/mod.rs` 2 个、`pricing/sync.rs` 3 个。
  - 适配器：codex、dsh、hermes、workbuddy 各 1 个新测试，gemini / opencode / pi / claude 断言改为新口径。
  - search：`window_is_lifted_out_of_the_filters`。
  - 网关 `tests/usage.rs` 3 个，CLI `tests/usage.rs` 4 个（真二进制 + 本地 mock 价格源，`env_clear`，临时 `HOME` / `UNIFLO_HOME` / `UNIFLO_DATA_DIR`）。
- `scripts/verify.sh --e2e`（最终代码）：rustfmt、clippy `-D warnings`、16 个适配器 feature 单独编译、分支不变量（仅有 comet 分支命名警告）均通过；全量测试 166 passed / 0 failed；daemon smoke 通过；网页 e2e `{"ok":true,"passed":34,"failed":0}`；`verify: all checks passed`，exit 0。

### Scenario → 证据

| Scenario | 证据 | 结果 |
|---|---|---|
| usage 口径归一 | `common.rs::usage_helpers`；`claude.rs::full_turn_maps_every_record_kind`（Anthropic 形）；`codex.rs::usage_record_wins_and_repeated_token_counts_are_skipped`（OpenAI / Codex 形）；`gemini.rs::upserts_tool_completion_and_turn_end`（Gemini 形）；`workbuddy.rs::completions_usage_splits_cache_out_of_prompt`（completions 形）；opencode / pi / hermes 既有断言更新 | PASS |
| 费用计算与来源优先级 | `usage/mod.rs::cost_and_source_priority`；`catalog.rs::cost_formula_tiers_and_cache_write_fallback` | PASS |
| 模型名归一与近似匹配 | `catalog.rs::names_normalize_and_match`、`dated_catalog_ids_answer_undated_names` | PASS |
| 价格同步成功并按时间段生效 | CLI `price_sync_opens_a_time_segment_and_reprices_only_new_steps`；`sync.rs::small_change_opens_a_segment_at_observation_time` | PASS |
| 同步失败与安全阀 | CLI `sync_failures_and_safeguards_keep_prices_and_overrides`、`no_price_sync_sends_no_request`；`sync.rs` 另两项；`pricing/mod.rs::overrides_win_and_reload_on_change` | PASS |
| 多维聚合与目录树 | 网关 `usage_groups_add_up_project_dir_window_and_tz` | PASS |
| 每步用量与上下文占用 | 网关 `per_step_detail_turns_and_context` | PASS |
| 会话合计实时推送 | 网关 `session_usage_is_pushed_live`（2 s 内收到 envelope，steps +1、cost 更新） | PASS |
| CLI 与 REST 一致 | CLI `cli_and_rest_agree_and_table_has_cost_columns`；真实数据 cli==rest（harness / model / 会话明细） | PASS |
| 本机真实数据统计 | 上节真实数据证据：分组正常返回、只输出计数与金额、未定价模型列表、启动耗时 +5.7% | PASS |

### 未验证与残余风险

- 未验证：Linux / Windows（只在 macOS 跑过；`tz.rs` 的 IANA 时区名读系统 zoneinfo，没有 zoneinfo 的系统只能用 local、UTC 与固定偏移）。
- 未验证：LiteLLM 备源的真实拉取（本机网络 `curl: (35)`）；备源解析由单测 `upstream_formats` 与 mock 覆盖。
- 内存：有缓存时空闲 RSS 比改动前多约 130–150 MB，冷构建峰值约 491 MB。
- Claude 同名 agent id 的两个 transcript 合并为一个会话的用量（会话列表本身也只有一个 key）。
- 101 步旧 Codex 步骤没有模型，计入未定价。
- 近似匹配（当前真实数据 1 个模型）可能按旧版本价计价，以 `approx` 标出，可用 `overrides.json` 纠正。
- `cache::default_path()` 按 `dirs::cache_dir()`，不跟随 `UNIFLO_HOME`（既有行为）；真实数据测量通过伪 `HOME` 隔离。
- 网关测试临时目录下 FS watcher 不触发（既有引擎行为，未修改）。
