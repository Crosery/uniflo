# agent-access 规格

让 agent 和外部服务尽量简单地使用 Uniflo。提供以下几种入口：stdio MCP 服务器、随二进制分发的 Skill、`uniflo setup` 全局接入、恢复会话、agent 记忆与指令文件目录，以及接力上下文。同一能力无论经 REST、CLI `--json` 还是 MCP `structuredContent` 获取，数据都一致。

## MCP 服务器 `uniflo mcp`

- **协议**：stdio，每行一条 JSON-RPC 2.0 消息。
  - 支持的方法：`initialize`、`ping`、`tools/list`、`tools/call`、`notifications/initialized`。
  - 协商的协议版本：2025-11-25、2025-06-18、2025-03-26、2024-11-05。
  - 只声明 tools 能力。
  - 协议层手写，不引入 `rmcp`。
- **数据来源**：先通过 `UNIFLO_URL`（默认 `http://127.0.0.1:7311`）和 `UNIFLO_TOKEN` 连接守护进程。连不上时退回进程内索引，并在首个结果中注明"守护进程未运行，结果来自一次性索引"。
- **工具输出**：每个工具同时返回 `structuredContent`（JSON，与对应 REST 响应同构）和一段简短文本摘要。所有工具都标注 `readOnlyHint: true`。
- **工具清单**：

  | 工具 | 输入 | 对应接口 |
  |---|---|---|
  | `uniflo_sessions` | `q`（会话搜索语法）、`limit`（1–100，默认 20） | `/v1/sessions` |
  | `uniflo_session` | `key`（必填）、`before`、`limit`（1–200，默认 60）、`max_text`、`include_tools`、`include_reasoning`、`around` | 会话详情与事件窗口 |
  | `uniflo_search` | `q`（必填）、`filter`、`limit`（1–30，默认 10） | `/v1/search` |
  | `uniflo_usage` | `group_by`、`q`、`since`、`until`、`under`、`depth`、`limit` | `/v1/usage` |
  | `uniflo_session_usage` | `key` | `/v1/sessions/{key}/usage` |
  | `uniflo_models` | — | `/v1/models` |
  | `uniflo_resume` | `key` | `/v1/sessions/{key}/resume`，只返回命令，不执行 |
  | `uniflo_memory` | `cwd`（可选）、`path`（可选，读取单个文件） | `/v1/memory` |
  | `uniflo_context` | `cwd`（必填）、`limit`、`since` | 与 `uniflo context` 相同 |
  | `uniflo_status` | — | `/v1/health` 加 `/v1/harnesses` |

  - 超出范围的参数值会被夹到边界，不报错。
  - 未知工具或参数类型错误返回 `-32602`；未知方法返回 `-32601`；JSON 无效返回 `-32700`。
  - key 不存在时，返回带 `isError: true` 的正常结果。
- **会话引用格式**：`uniflo://session/<key>#<event-id>`。`uniflo_session` 的 `key` 参数接受这种形式。

## Skill

- `SKILL.md` 通过 `include_str!` 编进二进制。内容说明如何用 `uniflo` CLI（`--json`）和 REST 查询会话、检索、用量、恢复命令和上下文。
- `uniflo skill print` 输出这份内容。

## `uniflo setup` 全局接入

- **交互流程**（stdin 与 stdout 都是 TTY 时）：
  1. 选择接入方式：MCP（默认）、Skill、两者、跳过。
  2. 从检测到的 harness 中多选，默认全选。
  3. 若检测到 Claude Code，询问是否安装 `uniflo context` 的 SessionStart hook，默认否。
  4. 展示将要改动的文件，确认后执行。
- **非交互**：只打印将执行的命令和改动，不写任何文件。
- **参数**：`--mcp`、`--skill`、`--both`、`--none`、`--agents a,b`、`--hook`、`--yes`、`--dry-run`、`--uninstall`、`--reload`。环境变量 `UNIFLO_SETUP=mcp|skill|both|none`、`UNIFLO_NO_SETUP=1`。
- **检测**：可执行文件在 PATH 中，或配置目录存在，即视为已安装。两者都不满足就跳过，且不创建目录。
- **MCP 注册**：
  - 有官方命令的 harness 用命令注册：
    - `claude mcp add --scope user uniflo -- <uniflo 绝对路径> mcp`
    - `codex mcp add uniflo -- …`
    - `gemini mcp add -s user …`
    - `droid mcp add uniflo --type stdio -- …`
    - `codebuddy mcp add -s user …`
    - `qodercli mcp add -s user …`
  - 其他 harness 直接编辑配置文件：
    - cursor：`~/.cursor/mcp.json`
    - omp：`~/.omp/agent/mcp.json`
    - opencode：`~/.config/opencode/opencode.json` 的 `mcp` 字段，`command` 为数组
    - kimi：`~/.kimi-code/mcp.json`
    - kiro：`~/.kiro/settings/mcp.json`
    - copilot：`~/.copilot/mcp-config.json`
    - qwen：`~/.qwen/settings.json`
  - 编辑规则：
    - JSON 用保序方式改写（`serde_json/preserve_order`）。
    - TOML 在没有 `codex` 命令时用 `toml_edit` 保格式编辑。
    - 只增改名为 `uniflo` 的条目。若该名字已被非 Uniflo 写入的条目占用，不覆盖，只给出提示。
    - 文件内容确实会变化时，才在原文件旁生成 `.bak-uniflo-<时间戳>` 备份，并原子写入。
    - 文件解析失败（例如 JSON 带注释、或校验报错）时，跳过该 harness 并报告原因，不尝试修复。
  - pi 的内置 MCP 已禁用，只装 Skill。
- **Skill 安装**：
  - 真源写到 `~/.agents/skills/uniflo/SKILL.md`。
  - 只为不读 `~/.agents/skills` 的 harness 建相对软链：claude、pi、cursor、droid、qoder、codebuddy、kiro、copilot、grok 等。
  - 读该目录的 harness（codex、gemini、opencode、omp、kimi）不再建软链，避免重复加载。
- **状态记录**：写在 `<配置目录>/setup.json`，`UNIFLO_HOME` 覆盖时随之变化。内容包括：所选方式、每个 harness 的结果、写过的文件、建过的软链、做过的备份。
  - `--uninstall` 按这份记录对称撤销：移除 `uniflo` 条目；只删除指向 Uniflo Skill 的软链；保留备份文件；用户自己的条目原样保留。
  - `--reload` 为新检测到的 harness 补接入，并把已接入条目中的可执行文件路径刷新为当前路径。
- **自动触发**：
  - `uniflo update` 成功结束、且处于 TTY 时：
    - 从未配置过：完整询问一次。
    - 已配置过：静默执行 reload，只对新检测到的 harness 询问。
  - 首次在 TTY 中运行任意非 daemon 的 uniflo 命令，且从未询问过：提示一次。拒绝的结果会被记住，不再追问。
  - 守护进程、MCP 服务器、非 TTY 环境从不提问。
- **SessionStart hook**（可选）：在 `~/.claude/settings.json` 的 `hooks.SessionStart` 中追加一条命令：`uniflo context --cwd "$CLAUDE_PROJECT_DIR" --limit 5 --since 14d 2>/dev/null || true`。按 uninstall 记录可以撤销。

## 恢复会话

- `GET /v1/sessions/{key}/resume` 返回：
  - `supported`
  - `argv`
  - `cwd`
  - `command`：按 POSIX shell 转义的一行命令
  - `command_powershell`
  - 不支持时给出 `reason`
- **会话 id 校验**：
  - 长度不超过 200；
  - 首字符为字母或数字；
  - 其余字符只能是字母、数字、`_`、`.`、`-`、`:`。
  - 不符合时，`supported=false`，`reason` 为"会话 id 不合法"。
  - 命令中的路径只经过转义拼接，任何内容都不会被当作 shell 片段。
- **各 harness 的恢复命令**：

  | harness | 恢复命令 | 是否需要 cwd |
  |---|---|---|
  | claude | `claude --resume <id>` | 是 |
  | codex | `codex resume <id>` | — |
  | qoder | `qodercli --resume <id>` | 是 |
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
  | 其余 harness | 不支持，返回 `supported=false` 并说明原因 | — |

- **`uniflo resume <key>`**：切换到 cwd 后执行 argv，继承当前终端。`--print` 只打印命令。不支持时退出码为 2，并说明原因。
- **`POST /v1/sessions/{key}/open-terminal`**（写接口，遵循 session-cleanup 规格中的写接口安全边界）：
  - macOS：可用 `terminal` 参数指定 `terminal`（默认）、`iterm`、`ghostty`；在新窗口中切换到 cwd 并执行恢复命令。脚本参数经过转义，命令文本不拼接进 AppleScript 源码。不依赖 macOS「自动化」（Apple 事件）权限：常驻守护进程由 launchd 启动，无法弹出授权，依赖它会在真实环境打不开窗口。打开失败时返回原因和可复制的命令。
  - 其他平台：返回 501，并附上可复制的命令。

## agent 记忆与指令文件

- `GET /v1/memory?cwd=<路径>` 只读列出以下文件，返回 `{path, scope: global|project, harness, bytes, updated_at}`：
  - **全局指令文件**：`~/.claude/CLAUDE.md`、`~/.codex/AGENTS.md`、`~/.gemini/GEMINI.md`、`~/.agents/AGENTS.md`、`~/.config/opencode/AGENTS.md`、`~/.omp/agent/AGENTS.md`、`~/.qwen/QWEN.md`、`~/.kimi-code/AGENTS.md`。
  - **Claude 项目记忆**：`~/.claude/projects/<slug>/memory/*.md`，只取与 cwd 对应的 slug。
  - **项目指令文件**：从 cwd 向上到 git 根之间的 `AGENTS.md`、`CLAUDE.md`、`GEMINI.md`、`QWEN.md`、`.cursorrules`、`.cursor/rules/*.mdc`。
- `GET /v1/memory/file?path=<路径>`：只允许读取上述列表中出现过的路径，内容上限 256 KB。其他路径返回 403。

## 接力上下文 `uniflo context`

- `uniflo context [--cwd .] [--limit 5] [--since 14d] [--json]` 输出该项目最近会话的简要 Markdown 列表。项目按 git 根匹配。
  - 每个会话一行：标题、harness、更新时间、key、首条输入预览（80 字以内）、费用。
  - 末尾给出用 MCP 或 CLI 读取完整会话的方法。
  - 守护进程不可用或没有匹配的会话时，输出为空，退出码为 0，可以安全地放进 hook。
- 对应 MCP 工具 `uniflo_context`。
- **文档与 ADR**：`docs/api.md`、新增 `docs/agents.md`（MCP 工具、Skill、setup、resume、memory、context）、ADR-0007（setup 写 harness 配置），以及 `AGENTS.md` 契约中的例外条款，均同步更新。

### Scenario: MCP 握手与工具列表
- GIVEN 守护进程在运行，数据为合成数据
- WHEN 用脚本通过 stdio 依次发送 `initialize`（协议版本 2025-06-18）、`notifications/initialized`、`tools/list`
- THEN 返回协商后的协议版本，并列出上述 10 个工具，每个工具都有 inputSchema 和 `readOnlyHint: true`
- AND 发送未知方法、错误的参数类型、非法 JSON 时，分别返回 -32601、-32602、-32700

### Scenario: MCP 工具结果与 REST 一致
- GIVEN 同一份合成数据
- WHEN 通过 MCP 调用 `uniflo_sessions`、`uniflo_search`、`uniflo_usage`、`uniflo_session_usage`、`uniflo_session`，并请求对应的 REST 接口
- THEN 每个工具的 `structuredContent` 与 REST 响应中对应的数值和 id 完全一致
- AND `limit=1000` 被夹到上限，不报错
- AND key 不存在时返回 `isError: true`

### Scenario: MCP 在守护进程不可用时退回本地
- GIVEN 守护进程未运行，`UNIFLO_HOME` 指向合成数据
- WHEN 调用 `uniflo_sessions`
- THEN 返回结果来自进程内索引，文本中注明"守护进程未运行"

### Scenario: 真实 harness 接入（隔离 HOME）
- GIVEN 一个临时 HOME，其中只放入 Claude Code 与 Codex 的最小配置；真实的 `claude` 与 `codex` 可执行文件
- WHEN 运行 `uniflo setup --mcp --agents claude,codex --yes`，再运行 `claude mcp list` 与 `codex mcp list`
- THEN 两个列表都显示 `uniflo`，`claude mcp list` 的健康检查为已连接
- AND 再次执行 setup 不产生重复条目，也不再生成备份
- AND `uniflo setup --uninstall` 之后两处的 `uniflo` 条目都被移除，其余条目与原文件一致
- AND 用户真实 HOME 不受影响

### Scenario: 直接编辑配置文件与 Skill 软链（隔离 HOME）
- GIVEN 临时 HOME 中有以下内容：`~/.cursor/mcp.json`（已有其他服务器，键的顺序是特定的）、`~/.omp/agent/mcp.json`、`~/.config/opencode/opencode.json`、一个带注释（无法解析）的 `~/.qwen/settings.json`、`~/.claude/skills/`，以及已被用户占用的 `uniflo` 条目的某个文件
- WHEN 运行 `uniflo setup --both --yes`
- THEN cursor、omp、opencode 中新增 `uniflo` 条目，原有条目和键的顺序保持不变，并各自生成一份备份
- AND opencode 条目中的 `command` 是数组
- AND qwen 被跳过，并报告"解析失败"
- AND 被占用的 `uniflo` 条目不被覆盖，只给出提示
- AND `~/.agents/skills/uniflo/SKILL.md` 是真实文件，`~/.claude/skills/uniflo` 是指向它的相对软链，codex 和 gemini 不建软链
- AND `--uninstall` 之后只删除指向 Uniflo 的软链和 `uniflo` 条目

### Scenario: 非交互、dry-run 与触发时机
- GIVEN 非 TTY 环境
- WHEN 运行 `uniflo setup`、`uniflo setup --dry-run`（在本机真实 HOME 上），以及 `uniflo update` 的收尾逻辑（通过单元测试模拟三种状态：从未配置、已配置、出现新 harness）
- THEN 非 TTY 环境不写任何文件，只打印计划
- AND 在真实 HOME 上执行 dry-run 时列出检测到的 harness 和计划中的改动，真实配置文件的修改时间不变
- AND update 收尾的行为与规格一致：从未配置时完整询问；已配置时静默 reload；出现新 harness 时只询问新 harness
- AND 守护进程和 `uniflo mcp` 从不进入询问流程

### Scenario: 恢复命令与注入防护
- GIVEN 以下合成会话：claude、codex、opencode、kimi、dsh 各一个；一个 id 为 `--dangerously-skip-permissions` 的会话；一个 cwd 中含空格和单引号的会话
- WHEN 请求 `/v1/sessions/{key}/resume`，并运行 `uniflo resume <key> --print`
- THEN 支持的 harness 返回与表格一致的 argv
- AND cwd 中含空格和引号的会话，`command` 在 sh 中执行时得到的 argv 与原值逐字一致
- AND 以 `-` 开头的 id 返回 `supported=false`，原因为"会话 id 不合法"
- AND dsh 返回 `supported=false`，并给出原因
- AND 对不支持的会话，`uniflo resume` 的退出码为 2

### Scenario: macOS 打开终端
- GIVEN macOS，一个 cwd 存在的合成 claude 会话
- WHEN 发送带写请求头的 `POST /v1/sessions/{key}/open-terminal`，参数分别为 `terminal=terminal` 和 `terminal=iterm`（iTerm 已安装时）
- THEN 新终端窗口打开在该 cwd，并执行 `claude --resume <id>`。这一项人工核对，并截图留证
- AND 缺少写请求头时返回 403
- AND 在非 macOS 平台上运行单元测试，返回 501 并附带命令

### Scenario: 记忆文件与接力上下文
- GIVEN 合成的 HOME 与项目目录，含全局 `CLAUDE.md`、项目 `AGENTS.md`、`.cursor/rules/a.mdc`、Claude 项目记忆文件；该项目还有 3 个最近会话
- WHEN 请求 `GET /v1/memory?cwd=<项目子目录>` 和 `GET /v1/memory/file?path=…`，运行 `uniflo context --cwd <项目子目录>`，并在守护进程停止时再运行一次
- THEN memory 列表中包含上述文件，scope 正确
- AND 读取列表中的文件返回内容；读取不在列表中的路径返回 403
- AND context 输出 3 行会话摘要以及读取方法
- AND 守护进程停止时 context 输出为空，退出码为 0

### Scenario: 三个入口数据一致
- GIVEN 同一份合成数据
- WHEN 对 usage、search、sessions 三类查询，分别通过 REST、CLI `--json` 和 MCP `structuredContent` 获取结果
- THEN 三者的数值与 id 完全一致
