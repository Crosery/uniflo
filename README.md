# Uniflo

![Uniflo：所有 agent 的会话，汇成一条实时流](docs/assets/banner.jpg)

[![crates.io](https://img.shields.io/crates/v/uniflo.svg)](https://crates.io/crates/uniflo) [![docs](https://img.shields.io/badge/docs-crosery.github.io%2Funiflo-9aa4ff.svg)](https://crosery.github.io/uniflo/) [![license](https://img.shields.io/badge/license-MIT-3fcf8e.svg)](LICENSE)

本机常驻的守护进程：持续读取 Claude Code、Codex、Gemini CLI、OpenCode、omp 等 33 个 AI coding agent 的会话，实时归一成一套格式，经本地网关（REST / SSE / NDJSON / WebSocket）推给你的应用、网页和 agent。默认只读，只监听本机。

[开发者站](https://crosery.github.io/uniflo/) · [快速开始](#快速开始) · [网页控制台](docs/console.md) · [网关 API](docs/api.md) · [Rust SDK](docs/sdk.md) · [Agent 接入](docs/agents.md) · [llms.txt](https://crosery.github.io/uniflo/llms.txt)

![会话视图](docs/assets/demo.png)

## 为什么

每个 agent harness 都把会话存成自己的格式：JSONL、整份 JSON、SQLite，字段、回合标记、子代理布局各不相同。想做一个"看所有 agent 在干什么"的面板，就得写 N 套解析、处理流式改写、自己判断谁在工作。Uniflo 只做这一层：

- **统一格式**：`Session` + `Event`（`user_message` / `assistant_message` / `reasoning` / `tool_call` / `tool_result` / `turn_start` / `turn_end` / `usage` / `system`），一套解析对接全部 harness。见 [`docs/schema.md`](docs/schema.md)。
- **两态**：每个会话只有 `work` 和 `idle`，规则统一，结合进程存活与超时。
- **实时**：文件追加到客户端收到事件通常在 10–50 ms；SSE、NDJSON、WebSocket 三种传输，带全局 `seq` 断线续传。
- **毫秒级查询**：头尾摘要 + 游标跟随 + 索引缓存；本机 5600+ 会话、1.8 GB 数据，有缓存时启动约 0.2 s，REST 请求个位数毫秒。
- **fd / fzf 式搜索**：`s:work h:claude in:uniflo since:2h 'gateway`。见 [`docs/search.md`](docs/search.md)。
- **全文检索**：按中文词或代码子串查所有会话的正文、思考、工具参数与输出，命中直达事件上下文：`uniflo grep 缓存击穿`、`/v1/search`。索引在后台构建；近 7 天的会话放热层，更早的压缩成 zstd 块，结果无损，本机实测 6.1 GB → 1.9 GB。见 [`docs/search.md#全文检索`](docs/search.md#全文检索)、[ADR-0011](docs/decisions/ADR-0011-全文索引紧凑格式.md)。
- **用量与费用**：每次模型调用的 token 统一口径（input 不含缓存、output 含思考），按 harness / 模型 / 项目 / 目录 / 日期 / 会话聚合，按事件时间的价格段计算 API 等价成本；未知模型单独计数，不按 0 计。见 [`docs/api.md`](docs/api.md#v1usage-参数)、[ADR-0010](docs/decisions/ADR-0010-价格目录同步与费用口径.md)。
- **agent 也能用**：stdio MCP 服务器 `uniflo mcp` 和随二进制分发的 Skill，`uniflo setup` 一条命令接入本机检测到的 harness；另有恢复会话、记忆与指令文件、接力上下文。见 [`docs/agents.md`](docs/agents.md)。
- **会话清理**：确认后把旧会话移入系统回收站释放空间，先写精简归档（zstd，去掉图片与大段输出），归档会话照常列出、检索、计入用量；从回收站还原即恢复。见 [`docs/api.md#会话清理`](docs/api.md#会话清理)、[ADR-0006](docs/decisions/ADR-0006-会话清理与写接口.md)。
- **只读、只听本机**：默认不写 harness 数据，例外只有你确认的会话清理（上一条）和 `uniflo setup` 改 harness 的 MCP 配置（可撤销，见 [ADR-0007](docs/decisions/ADR-0007-setup-写-harness-配置.md)）；网关校验 Host / Origin，可选 token。

## 快速开始

### 1. 安装

三种方式任选其一：

```sh
# 方式 1：crates.io（推荐给 Rust 用户，从源码编译）
cargo install uniflo

# 方式 2：一行安装脚本（无需 Rust；下载预编译包，校验 SHA-256 后装到 ~/.local/bin）
curl -fsSL https://github.com/Crosery/uniflo/releases/latest/download/install.sh | sh
#   Windows PowerShell（装到 %LOCALAPPDATA%\Programs\uniflo）：
#   irm https://github.com/Crosery/uniflo/releases/latest/download/install.ps1 | iex

# 方式 3：cargo-binstall（下载同一份预编译包，不编译）
cargo binstall uniflo
```

预编译包覆盖 macOS（arm64 / x86_64）、Linux（x86_64 / aarch64，静态 musl）、Windows（x86_64），随每个 GitHub Release 发布，附 `SHA256SUMS`。安装脚本可用的环境变量：`UNIFLO_VERSION`（指定版本，默认最新正式版）、`UNIFLO_INSTALL_DIR`（安装目录）、`UNIFLO_NO_SETUP=1` 或 `sh -s -- --no-setup`（不运行 `uniflo setup`）。在终端里运行时，脚本最后会调用 `uniflo setup`，询问是否接入本机 agent。安装记录写在配置目录的 `install.json`，`uniflo update` 靠它识别安装方式。决策见 [ADR-0009](docs/decisions/ADR-0009-预编译分发与自升级.md)。

### 2. 启动守护进程，打开控制台

```sh
uniflo daemon                             # 后台静默启动网关，默认 http://127.0.0.1:7311
open http://127.0.0.1:7311/demo           # 打开网页控制台

uniflo daemon -f                          # 前台启动网关并实时输出日志（--foreground）
uniflo daemon --stop                      # 停止后台运行的守护进程
uniflo daemon --restart                   # 重启后台守护进程
uniflo daemon --no-fts                    # 不建全文索引（/v1/search 返回 503）
uniflo daemon --read-only                 # 关闭全部写接口（会话清理等返回 403）

# macOS 也可一键安装 launchd 后台常驻服务（登录自启）
scripts/install-service.sh
```

### 3. 接入 agent

```sh
uniflo setup                              # 终端里交互：选 MCP / Skill / 两者，选 harness，确认后执行
uniflo setup --mcp --agents claude,codex --yes   # 非交互（不加 --yes 只打印计划）
uniflo setup --dry-run                    # 只看检测结果和计划改动
uniflo setup --uninstall                  # 按记录撤销，备份保留
uniflo mcp                                # MCP 服务器本体（stdio），由 harness 启动
uniflo skill print                        # 输出内置 SKILL.md
uniflo resume claude:4f1c                 # 在会话自己的 harness 里继续它（--print 只打印命令）
uniflo context --cwd .                    # 本项目最近会话的 Markdown 摘要，可做 SessionStart hook
```

接入后 agent 可用 10 个只读工具：`uniflo_sessions`、`uniflo_session`、`uniflo_search`、`uniflo_usage`、`uniflo_session_usage`、`uniflo_models`、`uniflo_resume`、`uniflo_memory`、`uniflo_context`、`uniflo_status`。守护进程没在跑时 MCP 服务器在进程内建一次索引回答。支持的 harness、注册方式和撤销规则见 [`docs/agents.md`](docs/agents.md)。

## 网页控制台

守护进程在 `/demo` 提供单文件网页客户端（[`examples/web/index.html`](examples/web/index.html)，无外部运行时依赖、免构建，也可以复制出去改造）。六个视图：**会话**（三栏实时会话流与详情，子代理按层级挂在父会话下）、**用量**（费用走势、分组明细、目录下钻、导出 CSV）、**检索**（全文检索直达事件）、**管理**（清理计划与归档）、**洞察**（热力图、星期 × 小时、排行）、**接口**（填参数直接发送、看本机真实返回，附 curl / JavaScript / Rust SDK 写法）。另有 `⌘K` 命令面板、连接与诊断抽屉、跟随系统 / 浅色 / 深色主题，`?redact` 模糊全部正文、标题与路径便于录屏。完整说明见 [`docs/console.md`](docs/console.md)。

| 用量与费用 | 活跃度洞察 | 接口 |
|---|---|---|
| ![用量视图](docs/assets/usage.png) | ![洞察视图](docs/assets/insights.png) | ![接口视图](docs/assets/api.png) |

截图均为合成数据。

## 命令行

CLI 有守护进程时连它，否则 `--local` 进程内索引：

```sh
uniflo harnesses                          # 每个 harness 的会话数 / 工作中数量
uniflo ps                                 # 正在工作的会话
uniflo find "h:omp in:geek since:1d"      # 结构化过滤 + 模糊匹配
uniflo grep 缓存击穿 --filter h:claude    # 全文检索正文，按会话分组列出命中片段
uniflo show claude:4f1c                   # 会话详情（key 前缀即可）
uniflo tail claude:4f1c -f                # 跟随一个会话的事件
uniflo watch --kinds tool_call            # 全局实时事件流
uniflo ls --tsv -n 500 | fzf              # 接 fzf
uniflo usage --by model --since 7d        # 按模型的 token 与费用（另有 harness/project/cwd/dir/day/hour/weekday/session）
uniflo usage --by dir --under ~/work      # 目录树下钻
uniflo usage claude:4f1c                  # 一个会话每一步的用量、费用与上下文占用
uniflo clean --query 'h:claude before:90d' --dry-run   # 只看清理计划：可释放多少、哪些不可清理及原因
uniflo clean claude:4f1c                  # 确认后归档并移入回收站（非交互加 --yes）
uniflo archive ls                         # 已清理的会话与归档大小；archive rm <key> 永久删除归档
```

提示和报错按系统语言显示：中文系统用中文，其他用英文。依次看 `UNIFLO_LANG`（`zh` / `en`）、`LC_ALL`、`LC_MESSAGES`、桌面语言（macOS「首选语言」、Windows 显示语言）、`LANG`，取第一个有值的；JSON、TSV 等机器输出不翻译。

## 接入自己的应用

任何语言都可以直接请求本地网关：

```js
const api = "http://127.0.0.1:7311";
const res = await fetch(`${api}/v1/sessions?q=s:work`);
const seq = res.headers.get("x-uniflo-seq");          // 快照对应的序号
render(await res.json());

const es = new EventSource(`${api}/v1/stream?since=${seq}`);   // 从快照之后无缝续上
es.addEventListener("session", (m) => upsertSession(JSON.parse(m.data).session));
es.addEventListener("event", (m) => upsertEvent(JSON.parse(m.data).event)); // 同 id 覆盖 = 流式更新
```

Rust 用 `cargo add uniflo-client`：

```rust
let client = uniflo_client::Client::local()?;
for s in client.sessions("s:work", 20)? {
    println!("{} · {}", s.key, s.title.unwrap_or_default());
}
```

完整接口见 [`docs/api.md`](docs/api.md)，SDK 选型与嵌入式用法见 [`docs/sdk.md`](docs/sdk.md)；控制台的「接口」视图可以直接试每个接口。

## 升级

守护进程默认每小时向 crates.io 稀疏索引发一次 HTTPS GET（系统 `curl`）。**更新只指向正式版**；预发布版（`-rc`/`-beta`）仅被探测并提示，装不装由你决定。在终端里运行 uniflo 的命令，结束时如果已有新正式版，会在末尾弹出一个小框提醒（stdout、stderr 都是终端时才出现，管道、脚本、hook、agent 看不到；`UNIFLO_NO_UPDATE_CHECK=1` 关闭）。最新版本缓存在缓存目录的 `update-check.json`，后台线程最多每小时刷新一次，命令本身不等网络。`uniflo update` 按安装方式升级：

```sh
uniflo update --check                     # 只查询有没有新正式版（退出码 10 = 有新版）
uniflo update --check --json              # 同上，JSON 另含 method：cargo | binary | unknown
uniflo update                             # 升级到最新正式版，并让守护进程换上新版本（见下）
uniflo update --pre                       # 显式选择：安装探测到的最新预发布版（不保证稳定）
uniflo daemon --no-update-check           # 关闭后台检查
```

| 安装方式 | 判定 | `uniflo update` 的做法 |
|---|---|---|
| `cargo` | 可执行文件在 `$CARGO_HOME/bin` 或 `~/.cargo/bin` | `cargo install uniflo --force --version <新版本>` |
| `binary` | `install.json` 记录的路径就是当前可执行文件（安装脚本写入） | 下载新版本的包和 `SHA256SUMS`，校验后原子替换，并更新 `install.json`；任何一步失败，原可执行文件都不变 |
| `unknown` | 其他（源码构建、拷贝的文件等） | 只打印上面两种升级方式，不做改动 |

装好之后由新版本收尾，每步一行：
- 守护进程还是旧版本时重启它：`uniflo daemon` 起的后台守护进程按原来的参数重启（启用了 token 时要先设置 `UNIFLO_TOKEN`）；macOS 上 launchd 服务运行的就是这个可执行文件时 `kickstart`；前台运行的只提示。已打开的 `/demo` 页面发现守护进程换了版本会自动刷新一次，视图和选中的会话保留（0.2.1 之前打开的页面需要手动刷新一次）。
- 接入过 agent 时刷新接入（只对新检测到的 harness 提问）；从没接入过时在终端里问一次。
- cargo 的编译日志收成一行进度，失败时才打印最后 40 行。

Windows 上 cargo 无法覆盖运行中的 exe：cargo 安装请先停掉 daemon 再执行 `uniflo update`。binary 安装会把运行中的 `uniflo.exe` 改名为 `uniflo.exe.old` 再放入新版本，`.old` 在下次启动 uniflo 时删除。

## 价格目录

费用按内置价格快照计算；守护进程启动约 1 分钟后、之后每 6 小时用系统 `curl` 拉一次 [models.dev](https://models.dev) 价格目录（LiteLLM 补缺），写到数据目录（macOS `~/Library/Application Support/uniflo/pricing/`）。请求不含任何会话数据。调价只对之后的用量生效；单项变化超过 50% 要连续两次读到才采用。官方厂商没有的模型取各转售方的中位价；名字带 harness 加的前缀或后缀时去掉再匹配（标 `approx`，不跨档位）。

```sh
uniflo pricing                            # 同步状态：来源、时间、是否过期、条目数
uniflo pricing sync                       # 立即同步
uniflo daemon --no-price-sync             # 完全不联网，只用内置快照与本地文件
```

自定义价格写 `pricing/overrides.json`（`[{"id":"my-model","prices":[{"from":0,"input":3,"output":15,"cache_read":0.3}],"context_limit":200000}]`，单位 USD / 百万 token），优先于目录，同步不会改它。

## 已支持的 harness

Claude Code、Qoder、Qwen Work、Codex、Pi、oh-my-pi、Crosery Agent、Command Code、Prime Agent、Cline、Roo Code、Kodu、Gemini CLI、Antigravity、OpenCode、Kilo Code、ZCode、MiMo Code、WorkBuddy、MiniMax Code、Hermes、Factory Droid、Reasonix、Cursor Agent、DeepSeek Harness、Grok CLI、Kiro CLI、Kimi Code、GitHub Copilot CLI、OpenClaw、CodeBuddy、Craft Agents、Devin CLI —— 共 33 个，存储位置与回合信号见 [`docs/adapters.md`](docs/adapters.md)。

## 项目结构

```text
crates/
  uniflo-schema    wire 类型（唯一对外契约）
  uniflo-core      适配器 trait、JSONL 驱动、状态机、引擎、缓存、监听、进程探测、用量账本与价格目录
  uniflo-search    查询语法 + 模糊排序；全文索引（SQLite FTS5，热层 + zstd 冷块）
  uniflo-adapters  每个 harness 一个模块 / 一个 cargo feature
  uniflo-gateway   REST / SSE / NDJSON / WebSocket + Host/Origin/token 守卫
  uniflo-client    连接运行中守护进程的 Rust 客户端
  uniflo-cli       uniflo 二进制：守护进程、查询客户端、MCP 服务器、setup
examples/web       网页控制台（同时由守护进程在 /demo 提供）
site/              开发者站（GitHub Pages），从 README 与 docs/ 生成
docs/              架构、契约、规范、决策记录
```

设计说明见 [`docs/architecture.md`](docs/architecture.md)，决策见 [`docs/decisions/`](docs/decisions/)。

## 开发

```sh
cargo install --path crates/uniflo-cli   # 从源码安装当前检出（装进 ~/.cargo/bin，uniflo update 按 cargo 方式处理，会换成 crates.io 上的正式版）
scripts/verify.sh          # fmt + clippy -D warnings + 每个适配器 feature 单独编译 + 全量测试
scripts/verify.sh --e2e    # 再用无头 Chrome 跑一遍网页控制台（需要 bun + Chrome）
```

开发规范：[`docs/conventions/DEVELOPMENT.md`](docs/conventions/DEVELOPMENT.md)；agent 协作规范：[`AGENTS.md`](AGENTS.md)。新增 harness：[`docs/adapters.md`](docs/adapters.md)。

## 许可

MIT
