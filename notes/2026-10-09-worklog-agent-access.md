# 2026-10-09 worklog · agent-access

> service-expansion 子任务 agent-access：stdio MCP 服务器 `uniflo mcp`、内置 Skill、`uniflo setup` 全局接入、恢复会话与 macOS 打开终端、agent 记忆与指令文件、接力上下文 `uniflo context`；写真实 harness 配置的验证只在临时 HOME 里做。

状态：`historical` · 更新：2026-10-09

## 23:30 · agent 接入

### 完成条件

- `docs/comet/changes/service-expansion/specs/agent-access/spec.md` 的 10 个 Scenario 都有自动化或真实二进制证据；做不到的写明"未验证"和原因。
- A5：临时 HOME 里用真实 `claude`、`codex` 跑 `uniflo setup --mcp`，`claude mcp list` 显示 uniflo 已连接；同一查询的 REST、CLI `--json`、MCP `structuredContent` 数值完全一致。
- 真实家目录只跑 `--dry-run` 和非交互的计划打印。每次真实二进制场景前后，用户真实配置文件的 sha256 和 mtime 不变（只记哈希与时间，不看内容）。
- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e` 通过；不碰 7311 上的守护进程，不打开真实终端窗口。

### 改动与依据

- schema（只增）：`crates/uniflo-schema/src/agent.rs` 新增 `ResumeInfo`、`MemoryScope`、`MemoryFile`、`MemoryContent`、`ContextReport`、`ContextSession`；`docs/schema.md#agent-接入接口的类型` 同步。
- core：
  - `resume.rs`：15 个 harness 的恢复命令表、会话 id 校验、POSIX / PowerShell 转义、三种终端的 `osascript` argv。命令行只作为 `on run argv` 的参数传入，不拼进 AppleScript 源码。
  - `memory.rs`：全局与项目指令文件、Claude 项目记忆的列举，以及无状态的读权限判断（`classify`）。
  - `context.rs`：按 git 根筛选项目的顶层会话，输出 Markdown。
- gateway：
  - `agent.rs`：`GET /v1/sessions/{key}/resume`、`POST …/open-terminal`、`GET /v1/memory`、`GET /v1/memory/file`；`Launcher` 可注入；`get_in_process` 用 `tower::ServiceExt::oneshot` 在进程内调同一个 router。
  - `guard.rs`：按约定逐字实现写接口守卫（`read_only`、`is_write`、`X-Uniflo-Write: 1`、CORS 方法与头）。没有加 `--read-only`、`/v1/health.read_only` 和 ADR-0006，那些属于 session-cleanup。
- cli：
  - `mcp.rs`：手写 JSON-RPC，共 10 个工具。守护进程不在时，退回进程内 Engine 加网关 router；用量账本、全文索引都按需建。
  - `agent.rs`：`uniflo resume`、`uniflo context`、`uniflo skill print`；`skill/SKILL.md` 用 `include_str!` 编进二进制。
  - `setup/`：`harness.rs` 是检测表（15 个 harness），`edit.rs` 负责 JSON / TOML / hook 编辑与原子写，`mod.rs` 负责流程、`setup.json` 记录、撤销与刷新，以及首次运行和 `uniflo update` 后的触发。
  - `main.rs` 只加子命令分派、首次运行钩子，以及 `update()` 成功路径末尾的一行 `setup::after_update()`。
- 依赖：
  - 新增 `toml_edit`（预批准，`default-features = false`，只开 `parse`、`display`）。
  - 全工作区开启 `serde_json` 的 `preserve_order`（预批准）。
  - 网关直接依赖 `tower`（`util`），它本来就在 axum 的依赖树里。
- 文档：
  - 新增 `docs/agents.md` 和 ADR-0007。
  - 更新 `docs/api.md`（新路由、写接口安全边界）、`docs/architecture.md`、`docs/conventions/DEVELOPMENT.md`、`docs/README.md`、`README.md`。
  - `AGENTS.md` 的契约里写明 setup 例外，以及只在临时 HOME 里测试的规则。

### 设计取舍（实现时定下的）

- MCP 的 `structuredContent` 必须是对象，所以 REST 返回数组的要包一层：`{sessions}`、`{models}`、`{files}`；`uniflo_status` 为 `{health, harnesses}`。
- `uniflo_session` 默认去掉工具与思考事件，用 `omitted` 计数，`include_tools` / `include_reasoning` 打开。
- setup 写入的 SessionStart hook 用 uniflo 的绝对路径（hook 环境里 PATH 不可靠），`--reload` 会刷新它。规格文字里的命令是 `uniflo context …`，这里只把程序名换成绝对路径，参数一致。
- pi、grok 没有 MCP 客户端，选 MCP 也会装 Skill。
- 恢复命令按 harness id 匹配。grok、kimi、copilot、codebuddy、devin 的适配器不在本分支的基线里，映射只做了单元测试。
- claude / qoder 的子代理会话没有独立的恢复入口，返回 `supported=false`，并指向父会话。
- 规格写 Claude 项目记忆"只取与 cwd 对应的 slug"，但 Scenario 是从项目子目录查、期望列出项目记忆，而 Claude Code 在仓库根启动时记忆挂在根目录的 slug 下。所以实现取从 git 根到 cwd 每一层的 slug，不在仓库里时只取 cwd。
- `uniflo context` 只连守护进程，不建一次性索引：放进 hook 时不能拖慢会话启动。没有守护进程时输出为空，退出码 0。
- 撤销前会再探测一次：已不是 Uniflo 写的条目，保留并提示。

### 失败与教训

- 幂等测试起初比较整个临时 HOME，`setup.json` 的 `updated_at` 每次都变，于是改为只比较 harness 文件。
- 撤销后按字节比较失败：JSON 重新渲染时，行内对象会展开。规格要求的是"其余条目与原文件一致"，测试改为比较语义和键的顺序。
- `codex mcp add/remove` 会把其他条目里的 `args = []` 省掉，撤销后 `config.toml` 字节不同、语义相同。这是 codex 自己的行为，已写进 `docs/agents.md` 和 ADR-0007。
- 全文检索的 `score` 带最近活动加权，随时间变化；三入口比对改为去掉 `score`，`hits`、`total`、会话 id 仍逐项相等。
- macOS 的 `script` 把管道输入当成 EOF，交互流程改用 Python pty 驱动（`/tmp/uniflo-aa/scripts/ptydrive.py`）核对。
- 第一版撤销结果显示成"connected"，后加 `Plan.undo`，改为显示"removed"；同一 harness 多个步骤的提示用"；"连起来，不再只显示最差的一条。
- 真实数据比对时，`since=30d` 每次请求按各自的"现在"计算，`since` 字段自然不同。比对脚本改为绝对日期。
- 教训：前一阶段用 `env | grep` 排查时，把一个令牌变量的值打到了输出里（没有写进任何文件）。之后只检查变量是否存在，不再打印环境变量的值。

### 真实家目录保护

指纹脚本 `/tmp/uniflo-aa/fingerprint.sh` 只输出以下内容：

- `~/.claude.json`、`~/.claude/settings.json`、`~/.codex/config.toml`、`~/.cursor/mcp.json` 等配置文件的 sha256 前 16 位和 mtime；
- `~/.agents/skills` 及各 harness skills 目录的条目名；
- claude `mcpServers` 的键名、codex `mcp_servers` 的表名。

| 场景 | 前 / 后指纹 | 结果 |
|---|---|---|
| 真实 `claude`、`codex` 隔离 HOME 测试（第一次） | `fp-1-before-real.txt` / `fp-1-after-real.txt` | `diff` 为空 |
| 真实家目录 `uniflo setup --dry-run` 与非交互 `uniflo setup` | `fp-2-before-dryrun.txt` / `fp-2-after-dryrun.txt` | `diff` 为空；`~/Library/Application Support/uniflo` 前后都不存在（没有写 `setup.json`） |
| 真实 `claude`、`codex` 隔离 HOME 测试（改撤销探测后重跑） | `fp-3-before-real.txt` / `fp-3-after-real.txt` | `diff` 为空 |
| `scripts/verify.sh --e2e`（不含真实 harness 场景） | `fp-4-before-verify.txt` / `fp-4-after-verify.txt` | 只有 `~/.claude.json` 的哈希和 mtime 变了，claude `mcpServers` 键名不变、没有 `uniflo`。同一文件在只改文档、不运行任何 uniflo 进程的时段里也变过（fp-3-after 1791557739 → fp-4-before 1791558389；verify 结束后只读日志时又变为 1791558560），是本机运行中的 Claude Code 会话自己在写 |

收尾时与本子任务最早的基线比较（`fp-0-before.txt` 对 `fp-5-final.txt`）：只有 `~/.claude.json` 的哈希和 mtime 不同，原因同上；其余配置文件、skills 目录条目、claude `mcpServers` 键名、codex `mcp_servers` 表名全部相同。

真实家目录只运行了 `--dry-run` 和不带 `--yes` 的非交互 `uniflo setup`。后者按设计只打印计划，但仍不在"只许 `--dry-run`"的字面范围内，所以单独记下：它没有写入任何文件，指纹与配置目录不变。

### 命令与结果

```sh
cargo fmt --all -- --check                                   # 通过
cargo clippy --workspace --all-targets -q -- -D warnings     # 0 警告
cargo test -q -p uniflo                                      # 24 + 5（1 ignored）+ 2 + 4 通过
cargo test -q -p uniflo --test agent -- --ignored real_ --nocapture   # 1 通过（真实 claude / codex）
CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e
# rustfmt、clippy、16 个适配器 feature 单独编译、分支不变量、全量测试（234 通过，1 ignored）、
# release 构建 + daemon-smoke（127.0.0.1:7399）、demo-e2e {"ok":true,"passed":34,"failed":0}；verify: all checks passed
```

真实 `claude`、`codex`（`real_claude_and_codex_setup_in_an_isolated_home`）：

- 隔离方式：`HOME=<临时目录>`、`UNIFLO_HOME=<临时目录>`，去掉 `CLAUDE_CONFIG_DIR` 和 `CODEX_HOME`；临时 HOME 里预置一个名为 `other` 的服务器（`/usr/bin/true`）。
- `uniflo setup --mcp --agents claude,codex --yes`：两项都是 `connected 已用 … mcp add 注册`。
- `claude mcp list`：`uniflo: …/target/debug/uniflo mcp - ✔ Connected`；`codex mcp list` 列出 `uniflo … mcp enabled`。
- 再执行一次：两项"未变化"，没有新备份。
- `uniflo setup --uninstall`：两项 `removed`。`~/.claude.json` 的 `mcpServers` 与原值完全相同；`config.toml` 语义相同（`codex config.toml after uninstall byte-identical to the original: false`，原因见上）。

真实家目录 dry-run（`./target/debug/uniflo setup --dry-run < /dev/null`）：检测到 claude、codex、gemini、factory、codebuddy、qoder、cursor、omp、opencode、kimi、kiro、copilot、pi、grok，共 14 个，列出 6 条 `mcp add` 命令、6 个配置文件编辑（cursor、omp、opencode、kimi、kiro、copilot）、Skill 文件和 2 个 Skill 软链（pi、grok），末尾"（预演：未写入任何文件）"，退出码 0。

终端与 pty 核对（临时 HOME，`/tmp/uniflo-aa/scripts/ptydrive.py`）：

- 交互：选两者、全部 harness、hook 选是、确认后执行。hook 写入了绝对路径，软链为 `../../.agents/skills/uniflo`。
- 首次运行提示回答 n：`setup.json` 为 `{asked: true, mode: null}`，再运行不再询问。
- `CI=true` 时不提示；`uniflo skill print` 不写记录。

真实数据（只读）：隔离的守护进程 `HOME=/tmp/uniflo-aa/rd/home UNIFLO_HOME=<真实家目录> UNIFLO_DATA_DIR/CONFIG_DIR/CACHE_DIR=/tmp/uniflo-aa/rd/*`，绑定 `127.0.0.1:7431`，参数 `--no-fts --no-update-check --no-price-sync`。

- `/v1/stats`：sessions 6576、`bad_lines 0`、`read_errors 0`、`unknown {}`，用量索引 5836/5836 就绪。
- 三入口比对（`/tmp/uniflo-aa/scripts/realdata_agree.py`，窗口 `2026-09-09` 到 `2026-10-09`，只打印计数与合计）：见下一节。

### 真实数据三入口比对：浮点末位不一致（已修复，见 23:50 一节）

同一守护进程上，`realdata_agree.py` 比较 REST、MCP `structuredContent`、CLI `--json`。窗口截止到当天零点，正在写入的会话不会改变数字。

| 查询 | 规模 | 未开 `float_roundtrip`（`2f06e5f`） | 实验构建（多开 `float_roundtrip`） |
|---|---|---|---|
| `usage` 按 harness / model / project | 15 / 50 / 171 行，272122 步，3642 个会话，$19101.15，未定价 51679 步 | REST≠MCP | REST==MCP==CLI |
| `ls` / `uniflo_sessions`（`before:2026-10-09 h:claude`） | 50 个 | REST≠MCP，REST≠CLI；key 一致 | 全部相等 |
| `uniflo_session_usage` | 373 步 | ≠ | 相等 |
| `uniflo_models` | — | ≠ | 相等 |

- 定位（`/tmp/uniflo-aa/scripts/diffpaths.py`，只输出 JSON 路径和相对差）：差异只出现在 `cost_usd` 浮点上，相对差 ≤ 2.1e-16，也就是最后一位。例如 50 个会话中有 4 个的 `usage.cost_usd`，49 个模型行中有 4 行。
- 原因：`serde_json` 默认的浮点解析不保证与原值逐位一致，凡是"解析再序列化"的路径都会有末位差，包括 MCP 和 CLI 的 `ls --json`（后者是既有行为）。usage-cost 子任务也遇到过，当时让 `uniflo usage --json` 原样转发响应字节绕过。合成数据的金额恰好能逐位往返，所以自动化测试没有暴露。
- 实验：在工作区之外的拷贝（`/tmp/uniflo-aa/fr`）里把 `serde_json` 特性改为 `["preserve_order", "float_roundtrip"]`，release 构建后对同一守护进程重跑，全部相等。工作区没有改动。
- `float_roundtrip` 不在预批准清单里，于是向 team-lead 申请。另一条路是不开特性，把 MCP 回复改成拼接 REST 原始字节、`ls --json` / `grep --json` 也原样转发：要多写约 100 行，`ls --json` 也会从多行缩进的格式变成单行紧凑格式。
- 真实数据上的全文检索（`uniflo_search`）没有比对：在隔离数据目录里建一次全量索引要 13 min 以上（见 fulltext-search 的 worklog），这里只用合成数据验证。

### Scenario → 证据

| Scenario | 证据 | 结论 |
|---|---|---|
| MCP 握手与工具列表 | `crates/uniflo-cli/tests/agent.rs::mcp_cli_and_rest_agree`：守护进程运行时 `initialize`（2025-06-18）回同一版本；10 个工具都有 `inputSchema` 和 `readOnlyHint: true`；未知方法 -32601，参数类型错 -32602，非法 JSON -32700。`mcp.rs` 单元测试另测批量请求、通知不回复、参数夹取 | PASS |
| MCP 工具结果与 REST 一致 | 同上测试：sessions、search（去掉 `score`）、usage、session_usage、session、resume、models、status 与 REST 逐项相等；`limit: 1000` 夹到上限；不存在的 key 返回 `isError: true` | PASS（真实数据见下文 23:50 一节，0 处不同） |
| MCP 在守护进程不可用时退回本地 | `mcp_answers_in_process_without_a_daemon`：文本以"守护进程未运行"开头且只提示一次；sessions、usage、search、status 都从进程内索引返回 | PASS |
| 真实 harness 接入（隔离 HOME） | `real_claude_and_codex_setup_in_an_isolated_home`（ignored，手动运行两次）加指纹 fp-1、fp-3 | PASS |
| 直接编辑配置文件与 Skill 软链（隔离 HOME） | `setup/tests.rs::both_edits_files_links_skill_and_uninstalls_symmetrically`、`uninstall_only_touches_what_is_still_uniflos`、`relative_links`：cursor、omp、opencode 新增条目，键序保留，各一份备份；opencode `command` 为数组；qwen 报"解析失败"；被占用的条目只提示；`SKILL.md` 是真实文件，claude 是相对软链，codex、gemini 无软链；撤销只删 Uniflo 的软链和条目 | PASS |
| 非交互、dry-run 与触发时机 | `tests/agent.rs::setup_outside_a_terminal_writes_nothing_and_nothing_prompts`；`setup/tests.rs::without_a_terminal_only_the_plan_is_printed`、`first_run_prompt_gating`、`after_update_three_states`、`after_update_refreshes_silently_and_asks_only_new`；真实家目录 dry-run 与指纹 fp-2 | PASS |
| 恢复命令与注入防护 | `tests/agent.rs::resume_commands_and_injection_guard`：claude、codex、opencode 的 argv 与表一致；cwd 含空格和单引号时，`sh -c command` 得到的 pwd 与 argv 逐字一致；`--dangerously-skip-permissions` 返回"会话 id 不合法"；dsh 不支持并给出原因；`uniflo resume` 退出码 2（守护进程与 `--local` 两种模式）。`core::resume` 单元测试覆盖整张表（含 kimi）。网关侧见 `crates/uniflo-gateway/tests/agent.rs` | PASS；kimi 的端到端（真实 kimi 适配器的会话）未验证：kimi 适配器随 harness-adapters 子任务合入，集成后补测 |
| macOS 打开终端 | `crates/uniflo-gateway/tests/agent.rs::open_terminal_needs_the_write_header_and_passes_the_line_as_an_argument`：缺头或头值不对 403，恶意 Origin 403，GET 405，`terminal=terminal` 时注入的启动器收到精确的 `osascript` argv，非法 id 422，只读 403 `read-only`，token 401；`preflight_allows_the_write_header`；`agent.rs` 单元测试覆盖 terminal / iterm / ghostty 三个脚本的 argv，以及非 macOS 的 501 加命令 | 自动化部分 PASS。真实窗口打开在 cwd 并执行 `claude --resume <id>`：未验证（待用户人工核对）。本机未装 iTerm；非 macOS 的 501 只有单元测试（`Env.macos=false`），没有在其他平台上实际运行 |
| 记忆文件与接力上下文 | `tests/agent.rs::memory_files_and_context`：全局 `CLAUDE.md`、项目 `AGENTS.md`、`.cursor/rules/a.mdc`、Claude 项目记忆都在列表中且 scope 正确；读取列表中的文件返回内容，越界路径 403；`uniflo context` 输出 3 行加读取方法；守护进程停止后输出为空、退出码 0；MCP `uniflo_context`、`uniflo_memory` 与 CLI、REST 相同 | PASS |
| 三个入口数据一致 | 合成数据：`mcp_cli_and_rest_agree` 中 usage、search、sessions 三入口相等（search 去掉 `score`）。真实数据：开启 `float_roundtrip` 后 usage、sessions、session usage、models 共 11 组比较 0 处不同（见下文 23:50 一节）；search 只在合成数据上验证 | PASS |

## 23:50 · 开启 serde_json float_roundtrip

team-lead 批准方案 1，条件：只改根 `Cargo.toml` 一行，`Cargo.lock` 不能出现新包，记录原因，重跑 `verify.sh --e2e` 和真实数据三入口比对，并且 0 处不同。

- 改动：根 `Cargo.toml` 中 `serde_json = { version = "1", features = ["preserve_order", "float_roundtrip"] }`。原因写在 ADR-0007 的"后果"和 `docs/agents.md` 开头：解析再序列化后浮点逐位不变，REST、CLI、MCP 的数值才能一致。usage-cost 原样转发响应字节的做法保持不变。
- `Cargo.lock`：`git diff Cargo.lock` 为 0 行，没有新包；`serde_json` 仍是 1.0.151。
- `cargo tree -e features -i serde_json` 列出的已开启特性为：`default`、`float_roundtrip`、`indexmap`、`preserve_order`、`raw_value`（其他依赖本来就开着）、`std`。
- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e` 全部通过：
  - rustfmt、clippy、16 个适配器 feature 单独编译、分支不变量；
  - 234 个测试通过，1 个 ignored；
  - daemon-smoke（127.0.0.1:7399）；
  - demo-e2e `{"ok":true,"passed":34,"failed":0}`。
- 指纹 `fp-6-before-verify2.txt` 对 `fp-6-after-verify2.txt`：仍然只有 `~/.claude.json` 变化，原因同上。
- 真实数据：用这次 release 构建另起隔离守护进程，绑定 `127.0.0.1:7431`。`/v1/stats` 显示 sessions 6576，`bad_lines` 0，`read_errors` 0，`unknown {}`，用量 5836/5836。
  - `realdata_agree.py`：usage 按 harness / model / project（15 / 50 / 171 行，272122 步，3642 个会话，$19101.15）三入口相等；sessions 50 个，REST==MCP==CLI；session usage 373 步，REST==MCP；models REST==MCP。
  - `diffpaths.py` 共 11 组比较，每组都是 0 处不同：
    - usage 按 harness / model / project，各比 REST 与 MCP、REST 与 CLI；
    - sessions，比 REST 与 MCP、REST 与 CLI；
    - session usage，比 REST 与 MCP、REST 与 CLI；
    - models，比 REST 与 MCP。
  - 比对结束后停掉 7431；7311 的 `/v1/health` 仍为 200，没有碰过它。
