# agent 接入

> 让 agent 直接查询本机所有会话：stdio MCP 服务器 `uniflo mcp`、随二进制分发的 Skill、一条命令全局接入的 `uniflo setup`，以及恢复会话、记忆与指令文件、接力上下文。

状态：`current` · 更新：2026-10-10

同一能力有三个入口：REST（`docs/api.md`）、CLI `--json`、MCP `structuredContent`。MCP 工具内部就是对网关 REST 路由的 `GET`，所以三者的数值与 id 一致（`crates/uniflo-cli/tests/agent.rs` 的 `mcp_cli_and_rest_agree` 逐项比对）。浮点也逐位一致：`serde_json` 开启了 `float_roundtrip`，响应解析后再输出不会改动最后一位。唯一会随时间变的是全文检索的 `score`：它带最近活动加权，隔一段时间再查会略有变化，比对时去掉它。

## MCP 服务器 `uniflo mcp`

- **协议**：stdio，每行一条 JSON-RPC 2.0 消息，也接受批量数组。支持 `initialize`、`ping`、`tools/list`、`tools/call`，通知（`notifications/initialized` 等）不回复。协议版本按 `2025-11-25`、`2025-06-18`、`2025-03-26`、`2024-11-05` 协商，客户端给的不在列表里时回最新的。只声明 `tools` 能力。协议层手写在 `crates/uniflo-cli/src/mcp.rs`，没有引入 `rmcp`。
- **stdout 只有协议消息**：诊断写 stderr；服务器从不提问，也不触发 `uniflo setup` 的首次询问。
- **数据来源**：先连 `UNIFLO_URL`（默认 `http://127.0.0.1:7311`，带 `UNIFLO_TOKEN`）。连不上时在进程内建一次索引，用网关自己的路由回答（`uniflo_gateway::agent::get_in_process`，与守护进程同一段代码）；用量账本、全文索引在第一次需要时才建（全文索引最多等 20 s，未建完时结果带 `indexing: true`）。此时第一个结果的文本以"守护进程未运行，结果来自一次性索引"开头。`--local` 直接走进程内索引。
- **错误**：JSON 无效 `-32700`；不是请求对象 `-32600`；未知方法 `-32601`；未知工具、`arguments` 不是对象、参数类型不对（如 `limit: "many"`、`limit: 2.5`、布尔参数给字符串）、缺必填参数 `-32602`。数值越界不报错，夹到边界。会话不存在、守护进程返回错误时是正常结果，`isError: true`，`structuredContent` 为 `{error}`。
- **结果**：每个工具返回 `content`（一段文本摘要）和 `structuredContent`，工具都标注 `readOnlyHint: true`。`structuredContent` 必须是对象，所以 REST 返回数组的工具包一层。

| 工具 | 输入 | `structuredContent` | 对应 REST |
|---|---|---|---|
| `uniflo_sessions` | `q`（会话搜索语法）、`limit` 1–100，默认 20 | `{sessions: Session[]}` | `/v1/sessions?q=&limit=` |
| `uniflo_session` | `key`（必填，也接受 `uniflo://session/<key>#<event-id>`，以及唯一的 key / id 前缀）、`before`、`limit` 1–200 默认 60、`max_text` 默认 4000、`include_tools`、`include_reasoning`（默认都不含）、`around` | `{session, events, next_before, omitted: {tool_events, reasoning}, around?}` | `/v1/sessions/{key}` + `/v1/sessions/{key}/events` |
| `uniflo_search` | `q`（必填）、`filter`、`limit` 1–30 默认 10 | `SearchResponse` | `/v1/search` |
| `uniflo_usage` | `group_by`、`q`、`since`、`until`、`under`、`depth` 1–32、`limit` 1–1000 | `UsageReport` | `/v1/usage` |
| `uniflo_session_usage` | `key` | `SessionUsageDetail` | `/v1/sessions/{key}/usage` |
| `uniflo_models` | — | `{models: ModelUsage[]}` | `/v1/models` |
| `uniflo_resume` | `key` | `ResumeInfo`（只给命令，不执行） | `/v1/sessions/{key}/resume` |
| `uniflo_memory` | `cwd`（可选）、`path`（读一个文件） | `{files: MemoryFile[]}`；给 `path` 时 `MemoryContent` | `/v1/memory`、`/v1/memory/file` |
| `uniflo_context` | `cwd`（必填，绝对路径，可用 `~`）、`limit` 1–50 默认 5、`since` 默认 `14d` | `ContextReport` | 与 `uniflo context --json` 相同 |
| `uniflo_status` | — | `{health, harnesses}` | `/v1/health` + `/v1/harnesses` |

- `uniflo_session` 的 `events` 是 REST 那一页去掉工具与思考事件后的结果，`omitted` 给出去掉的条数；翻更早一页传 `before=next_before`。
- `uniflo_search` 的文本摘要里每条命中带一个 `uniflo://session/<key>#<event-id>` 引用，原样传给 `uniflo_session` 即打开命中处的上下文。

## Skill

`crates/uniflo-cli/skill/SKILL.md` 用 `include_str!` 编进二进制，`uniflo skill print` 输出它。内容是给 agent 的用法：会话 key、`uniflo ls/show/tail/grep/usage/resume/context --json`、对应的 REST 路由，以及守护进程不在时的行为。`uniflo setup --skill` 把它装到 `~/.agents/skills/uniflo/SKILL.md`。

## `uniflo setup`

只改 harness 的**配置**（MCP 条目、Skill 软链、可选的 hook），不碰会话数据。这是"只读 harness 数据"契约的两个例外之一（另一个是会话清理），边界见 `docs/decisions/ADR-0007-setup-写-harness-配置.md`。

### 流程与参数

- **交互**（stdin、stdout 都是终端）：选接入方式（MCP 默认 / Skill / 两者 / 跳过）→ 多选检测到的 harness（默认全选）→ 检测到 Claude Code 时问是否装 SessionStart hook（默认否）→ 列出要改的文件和命令 → 确认后执行。
- **非交互**：只打印计划，不写任何文件；要执行加 `--yes`。`--dry-run` 在任何环境下都只打印。
- 参数：`--mcp` / `--skill` / `--both` / `--none`（互斥）、`--agents claude,codex`、`--hook`、`-y/--yes`、`--dry-run`、`--uninstall`、`--reload`。环境变量 `UNIFLO_SETUP=mcp|skill|both|none` 等同对应参数；`UNIFLO_NO_SETUP=1` 关掉自动询问。
- **检测**：PATH 里有它的可执行文件，或它的配置目录存在，就算已安装；都没有就跳过，也不建目录。
- MCP 条目里写的是当前 `uniflo` 可执行文件的绝对路径（`canonicalize` 后），换了安装位置用 `--reload` 刷新。

### MCP 注册

有官方命令的 harness 用命令注册；已有一个指向别处的 `uniflo` 条目且是 Uniflo 写的，先 `remove` 再 `add`；名字被别人占用则不动，只提示。

| harness（id） | 注册命令 | 写入位置（探测用） |
|---|---|---|
| Claude Code（`claude`） | `claude mcp add --scope user uniflo -- <uniflo> mcp` | `~/.claude.json` 的 `mcpServers` |
| Codex（`codex`） | `codex mcp add uniflo -- <uniflo> mcp`；没有 `codex` 命令时用 `toml_edit` 改 `~/.codex/config.toml` 的 `[mcp_servers.uniflo]` | `~/.codex/config.toml` |
| Gemini CLI（`gemini`） | `gemini mcp add -s user uniflo <uniflo> mcp` | `~/.gemini/settings.json` |
| Factory Droid（`factory`，别名 `droid`） | `droid mcp add uniflo --type stdio -- <uniflo> mcp` | `~/.factory/mcp.json` |
| CodeBuddy（`codebuddy`） | `codebuddy mcp add -s user uniflo -- <uniflo> mcp` | `~/.codebuddy/.mcp.json` |
| Qoder（`qoder`，别名 `qodercli`） | `qodercli mcp add -s user uniflo -- <uniflo> mcp` | `~/.qoder/settings.json` |

其余 harness 直接编辑 JSON（`CLAUDE_CONFIG_DIR`、`CODEX_HOME` 设置时，claude / codex 的文件随之改变位置）：

| harness | 文件 | 条目 |
|---|---|---|
| Cursor | `~/.cursor/mcp.json` | `mcpServers.uniflo = {command, args: ["mcp"]}` |
| oh-my-pi | `~/.omp/agent/mcp.json` | `mcpServers.uniflo = {type: "stdio", command, args}` |
| OpenCode | `~/.config/opencode/opencode.json` | `mcp.uniflo = {type: "local", command: [<uniflo>, "mcp"], enabled: true}` |
| Kimi Code | `~/.kimi-code/mcp.json` | `mcpServers.uniflo = {command, args}` |
| Kiro | `~/.kiro/settings/mcp.json` | 同上 |
| GitHub Copilot CLI | `~/.copilot/mcp-config.json` | `mcpServers.uniflo = {type: "local", command, args, tools: ["*"]}` |
| Qwen Code | `~/.qwen/settings.json` | `mcpServers.uniflo = {command, args}` |

Pi、Grok 没有可用的 MCP 客户端，只装 Skill（选了 MCP 也会装）。

编辑规则：

- 只增改名为 `uniflo` 的条目。判断"是 Uniflo 写的"：条目运行的是名为 `uniflo` 的程序加唯一参数 `mcp`。
- JSON 用 `serde_json` 的 `preserve_order` 读写，原有键的顺序和缩进宽度保留；TOML 用 `toml_edit` 保留注释与格式。
- 内容确实变化时才在原文件旁留 `<文件>.bak-uniflo-<时间戳>`（同一秒内重复时加 `-2`、`-3`），再写临时文件并 `rename` 原子替换；配置是软链时改它指向的文件，保留权限位。
- 解析失败（如带注释的 JSON）就跳过该 harness 并报告"解析失败"，不尝试修复。
- 已知差异：走官方命令的 harness 由它自己的 CLI 改写配置，Uniflo 不另做备份；Codex 的 `codex mcp add/remove` 会把其他条目里的 `args = []` 省掉、把行内 `env = {…}` 展开成子表，所以撤销后 `config.toml` 与原文件语义相同、字节不一定相同。

### Skill 安装

真源写到 `~/.agents/skills/uniflo/SKILL.md`。只给不读 `~/.agents/skills` 的 harness 建**相对**软链 `<skills 目录>/uniflo → …/.agents/skills/uniflo`：claude（`~/.claude/skills`）、factory、codebuddy、qoder、cursor、kiro、copilot、qwen、pi（`~/.pi/agent/skills`）、grok。codex、gemini、opencode、omp、kimi 自己读 `~/.agents/skills`，不建软链，避免重复加载。同名的真实目录或指向别处的软链不动，只提示。

### 记录、撤销与刷新

- 记录写在 `<配置目录>/setup.json`（macOS `~/Library/Application Support/uniflo/setup.json`，随 `UNIFLO_CONFIG_DIR` / `UNIFLO_HOME` 变化）：所选方式、每个 harness 的结果与方式（命令 / JSON 路径与新建的容器 / TOML）、写过的文件、建过的软链、做过的备份、是否问过。
- `--uninstall` 按记录对称撤销：再次确认条目仍是 Uniflo 的才删（官方命令的 `mcp remove`，或从文件删掉 `uniflo` 条目，当初新建的空容器 / 文件一并删掉），只删指向 Uniflo Skill 的软链，备份全部保留，用户自己的条目原样不动。
- `--reload`：把记录里已接入的条目刷新到当前可执行文件，并为新检测到、没被拒绝过的 harness 补接入。
- 重复执行是幂等的：内容不变时显示"未变化"，不生成备份。

### 自动触发

- 第一次在终端里运行任意查询类命令（不含 `daemon`、`mcp`、`setup`、`skill`、`context`、`update`）且从未问过时，提示一次；回答"否"会记进 `setup.json`，不再追问。
- `uniflo update` 安装成功后（仅终端里）：从未配置过 → 完整询问一次；已配置 → 静默 `--reload`，只对新检测到的 harness 询问；选过"跳过"→ 什么都不做。
- 安装脚本（`install.sh` / `install.ps1`）结尾：在终端里运行 `uniflo setup`；`--no-setup`、`UNIFLO_NO_SETUP=1` 或非终端时跳过。
- `CI` 有值、`UNIFLO_NO_SETUP=1`、非终端、守护进程、MCP 服务器：从不提问。

### SessionStart hook（可选，仅 Claude Code）

在 `~/.claude/settings.json` 的 `hooks.SessionStart` 追加一组 `{hooks: [{type: "command", command}]}`，命令为：

```sh
<uniflo 绝对路径> context --cwd "$CLAUDE_PROJECT_DIR" --limit 5 --since 14d 2>/dev/null || true
```

路径含空格等特殊字符时按 POSIX 规则加单引号。

用绝对路径而不是 PATH 里的 `uniflo`，hook 运行环境的 PATH 不可靠；`--reload` 会刷新它。`--uninstall` 只删这一条。

### 测试与沙箱

设了 `UNIFLO_HOME` 时 setup 把它当家目录：探测、编辑都在其下，调用 `claude mcp add` 等命令时子进程的 `HOME` 指向它并去掉 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`。在真实家目录上只允许 `--dry-run`。

## 恢复会话

`GET /v1/sessions/{key}/resume` 返回 `ResumeInfo`：`supported`、`argv`、`cwd`、`command`（POSIX shell 一行）、`command_powershell`，不支持时 `reason`。规则在 `crates/uniflo-core/src/resume.rs`。

| harness | 命令 | 需要 cwd |
|---|---|---|
| claude | `claude --resume <id>`（子代理会话不能单独恢复） | 是 |
| codex | `codex resume <id>` | — |
| qoder | `qodercli --resume <id>`（子代理同上） | 是 |
| cursor | `cursor-agent --resume <id>` | — |
| opencode | `opencode --session <id>` | — |
| kilo | `kilo --session <id>` | 是 |
| pi | `pi --session <id>` | — |
| omp | `omp --resume <id>` | — |
| grok | `grok --resume <id>` | — |
| kimi | `kimi --session <id>` | — |
| copilot | `copilot --resume=<id>` | — |
| codebuddy | `codebuddy --resume <id>` | 是 |
| devin | `devin --resume <id>` | 是 |
| hermes | `hermes --resume <id>` | — |
| antigravity | `agy --conversation=<id>` | — |

- 其余 harness `supported=false`，`reason` 说明原因。按 harness id 匹配，所以之后接入的适配器（grok、kimi、copilot、codebuddy、devin…）无需改这张表。
- **会话 id 校验**：不超过 200 字节，首字符是 ASCII 字母或数字，其余只能是字母、数字、`_`、`.`、`-`、`:`；否则 `supported=false`，`reason` 为"会话 id 不合法"（例如 `--dangerously-skip-permissions`，不会被当成参数）。
- `command`：需要 cwd 的 harness 是 `cd <cwd> && <argv>`，其余是 argv 本身；只含 `[A-Za-z0-9_./:@%+,=-]` 的词原样输出，其余词用单引号包起来（`'` 写成 `'"'"'`），以 `=` 开头的词也加引号（避免 zsh 展开）；路径里的空格、引号、`$`、反引号都不会被当成 shell 片段。`command_powershell` 是 `Set-Location -LiteralPath '<cwd>' -ErrorAction Stop; & 'prog' 'arg'`。
- **`uniflo resume <key>`**：切换到 cwd 后 `exec` argv，继承当前终端；`--print` 只打印 `command`。不支持时退出码 2，stderr 说明原因；需要 cwd 而目录已不存在时报错。

### `POST /v1/sessions/{key}/open-terminal`

写接口，遵循写接口安全边界（见 ADR-0006 / `docs/api.md#写接口`）：回环 Host、Origin、`X-Uniflo-Write: 1`、配置了 token 时带 token，任一不满足 403；网关只读时 403。

- macOS：`terminal=terminal`（默认）、`iterm`、`ghostty` 选终端；在新窗口切到 cwd 并执行恢复命令，返回 `{opened, terminal, command, cwd}`。Terminal、iTerm 把一个自删除的 `.command` 脚本交给 `open -a`：脚本写在随机命名的私有临时目录（目录与文件权限 0700），内容是 `cd <cwd> && <恢复命令>`（均经 POSIX 转义）后接一个交互 shell，运行时先删除自身与目录；Ghostty 用 `open -na Ghostty --args --working-directory=<cwd> -e /bin/sh -c <命令>`，命令是单个 argv 元素（Ghostty 首次执行外部命令会弹出自带的确认框）。全程不使用 AppleScript / Apple events，**不需要「自动化」权限**，所以 launchd 下的守护进程也能开窗。iTerm、Ghostty 只在 `/Applications` 或 `~/Applications` 里找得到时可用。
- 不支持恢复 422（附 `resume`）；终端名无效 400；需要 cwd 而目录不存在、或所选终端没装 422；启动失败（`open` 非零退出、临时脚本写不了）500，返回人话 `reason`（同 `error`）并附可复制的 `command`。
- 其他平台 501，附 `command`、`command_powershell`、`cwd`，由调用方自己在终端里执行。

## 记忆与指令文件

`GET /v1/memory?cwd=<绝对路径>` 只读列出 agent 会加载的文件，每项 `MemoryFile{path, scope, harness, bytes, updated_at}`。不给 `cwd` 时只列全局文件。

- **全局**（`scope: global`）：`~/.claude/CLAUDE.md`、`~/.codex/AGENTS.md`、`~/.gemini/GEMINI.md`、`~/.agents/AGENTS.md`、`~/.config/opencode/AGENTS.md`、`~/.omp/agent/AGENTS.md`、`~/.qwen/QWEN.md`、`~/.kimi-code/AGENTS.md`。
- **项目**（`scope: project`）：从 git 根到 cwd 的每一层目录（不在 git 仓库里时只看 cwd 本身），依次列出该目录的 Claude 项目记忆 `~/.claude/projects/<slug>/memory/*.md`（slug 为路径中每个非字母数字字符换成 `-`；Claude Code 在仓库根启动时记忆挂在根目录的 slug 下，所以从子目录查也要带上它），以及 `AGENTS.md`、`CLAUDE.md`、`GEMINI.md`、`QWEN.md`、`.cursorrules`、`.cursor/rules/*.mdc`。

`GET /v1/memory/file?path=<绝对路径>` 返回 `MemoryContent`（`MemoryFile` 字段加 `content`，超过 256 KB 截断并带 `truncated: true`）。只读上面这些位置和文件名能产生的路径：相对路径、含 `.` / `..`、其他文件名一律 403；文件不存在 404。判断是无状态的，不需要先调 `/v1/memory`。

## 接力上下文 `uniflo context`

```sh
uniflo context [--cwd .] [-n/--limit 5] [--since 14d] [--json]
```

列出 `--cwd` 所在项目（git 根；不在仓库里时就是该目录）最近的顶层会话，按最近活动倒序，给新会话一个接力的起点：

```text
## Recent agent sessions in /Users/me/work/app

- 修复登录超时 · claude · 2026-10-09 14:02 · `claude:4f1c…` · "登录接口在高峰期超时，先看…" · $0.42
```

每行：标题、harness、本地时间、key、首条输入预览（不超过 80 字符，含省略号）、费用（未知时 `cost unknown`）；末尾一行说明怎么读完整会话（MCP `uniflo_session` / `uniflo_search`，CLI `uniflo show` / `uniflo tail` / `uniflo grep`）。只连守护进程，不在进程内建索引；守护进程不在、出错或没有匹配会话时输出为空、退出码 0，可以直接放进 hook。`--json` 输出 `ContextReport`。MCP 工具 `uniflo_context` 与它同一实现（`crates/uniflo-cli/src/agent.rs` 的 `build_context` + `uniflo_core::context`）。

`--cwd` 不解析软链（相对路径从 `$PWD` 算起），因为 harness 记录的是逻辑路径，如 `/tmp/x` 而不是 `/private/tmp/x`。
