# Uniflo

> 一个本地守护进程，把机器上所有 AI coding agent（Claude Code、Codex、omp、OpenCode、Gemini CLI…）的会话实时归一成一套格式，通过本地网关供桌面端 / 网页端直接接入。

![demo](docs/assets/demo.png)

## 为什么

每个 agent harness 都把会话存成自己的格式：JSONL、整份 JSON、SQLite，字段、回合标记、子代理布局各不相同。想做一个"看所有 agent 在干什么"的面板，就得写 N 套解析、处理流式改写、自己判断谁在工作。

Uniflo 只做这一层：

- **统一格式**：`Session` + `Event`（`user_message` / `assistant_message` / `reasoning` / `tool_call` / `tool_result` / `turn_start` / `turn_end` / `usage` / `system`），一套解析对接全部 harness。见 [`docs/schema.md`](docs/schema.md)。
- **两态**：每个会话只有 `work` 和 `idle`，规则统一，结合进程存活与超时。
- **实时**：文件追加到客户端收到事件通常在 10–50 ms；SSE、NDJSON、WebSocket 三种传输，带全局 `seq` 断线续传。
- **毫秒级查询**：头尾摘要 + 游标跟随 + 索引缓存；本机 5600+ 会话、1.8 GB 数据，有缓存时启动约 0.2 s，REST 请求个位数毫秒。
- **fd / fzf 式搜索**：`s:work h:claude in:uniflo since:2h 'gateway`。见 [`docs/search.md`](docs/search.md)。
- **用量与费用**：每次模型调用的 token 统一口径（input 不含缓存、output 含思考），按 harness / 模型 / 项目 / 目录 / 日期 / 会话聚合，按事件时间的价格段计算 API 等价成本；未知模型单独计数，不按 0 计。见 [`docs/api.md`](docs/api.md#v1usage-参数)、[ADR-0010](docs/decisions/ADR-0010-价格目录同步与费用口径.md)。
- **全文检索**：按中文词或代码子串查所有会话的正文、思考、工具参数与输出，命中直达事件上下文：`uniflo grep 缓存击穿`、`/v1/search`。索引在后台构建，见 [`docs/search.md#全文检索`](docs/search.md#全文检索)。
- **agent 也能用**：stdio MCP 服务器 `uniflo mcp` 和随二进制分发的 Skill，`uniflo setup` 一条命令接入本机检测到的 harness；另有恢复会话、记忆与指令文件、接力上下文。见 [`docs/agents.md`](docs/agents.md)。
- **只读、只听本机**：默认不写 harness 数据，例外只有你确认的会话清理（下一条）和 `uniflo setup` 改 harness 的 MCP 配置（可撤销，见 [ADR-0007](docs/decisions/ADR-0007-setup-写-harness-配置.md)）；网关校验 Host / Origin，可选 token。
- **会话清理**：确认后把旧会话移入系统回收站释放空间，先写精简归档（zstd，去掉图片与大段输出），归档会话照常列出、检索、计入用量；从回收站还原即恢复。见 [`docs/api.md#会话清理`](docs/api.md#会话清理)、[ADR-0006](docs/decisions/ADR-0006-会话清理与写接口.md)。

## 已支持的 harness

Claude Code、Qoder、Qwen Work、Codex、Pi、oh-my-pi、Crosery Agent、Command Code、Prime Agent、Cline、Roo Code、Kodu、Gemini CLI、Antigravity、OpenCode、Kilo Code、ZCode、MiMo Code、WorkBuddy、MiniMax Code、Hermes、Factory Droid、Reasonix、Cursor Agent、DeepSeek Harness、Grok CLI、Kiro CLI、Kimi Code、GitHub Copilot CLI、OpenClaw、CodeBuddy、Craft Agents、Devin CLI —— 共 33 个，存储位置与回合信号见 [`docs/adapters.md`](docs/adapters.md)。

## 安装与快速开始

### 安装

```sh
# 方式 1：通过 crates.io 安装
cargo install uniflo

# 方式 2：直接通过 GitHub 安装最新预发布版
cargo install --git https://github.com/Crosery/uniflo.git uniflo

# 方式 3：从源码安装
git clone https://github.com/Crosery/uniflo.git && cd uniflo
cargo install --path crates/uniflo-cli
```

### 启动服务

```sh
uniflo daemon                             # 前台启动网关，默认 http://127.0.0.1:7311
uniflo daemon --no-fts                    # 不建全文索引（/v1/search 返回 503）
uniflo daemon --read-only                 # 关闭全部写接口（会话清理等返回 403）
open http://127.0.0.1:7311/demo           # 打开内置网页演示

# macOS 可一键安装 launchd 后台常驻服务（登录自启、静默运行）
scripts/install-service.sh
```

### 升级

守护进程默认每小时向 crates.io 稀疏索引发一次 HTTPS GET（系统 `curl`）。**更新只指向正式版**；预发布版（`-rc`/`-beta`）仅被探测并提示，装不装由你决定：

```sh
uniflo update --check                     # 只查询有没有新正式版（退出码 10 = 有新版）
uniflo update                             # cargo install uniflo --force --version <最新正式版>，macOS 上尝试 kickstart 重启 launchd 服务
uniflo update --pre                       # 显式选择：安装探测到的最新预发布版（不保证稳定）
uniflo daemon --no-update-check           # 关闭后台检查
```

Windows 上运行中的 exe 无法被覆盖：先停掉 daemon 再执行 `uniflo update`。

### 价格目录

费用按内置价格快照计算；守护进程启动约 1 分钟后、之后每 6 小时用系统 `curl` 拉一次 [models.dev](https://models.dev) 价格目录（LiteLLM 补缺），写到数据目录（macOS `~/Library/Application Support/uniflo/pricing/`）。请求不含任何会话数据。调价只对之后的用量生效；单项变化超过 50% 要连续两次读到才采用。

```sh
uniflo pricing                            # 同步状态：来源、时间、是否过期、条目数
uniflo pricing sync                       # 立即同步
uniflo daemon --no-price-sync             # 完全不联网，只用内置快照与本地文件
```

自定义价格写 `pricing/overrides.json`（`[{"id":"my-model","prices":[{"from":0,"input":3,"output":15,"cache_read":0.3}],"context_limit":200000}]`，单位 USD / 百万 token），优先于目录，同步不会改它。

CLI（有守护进程时连它，否则 `--local` 进程内索引）：

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

## 接入 agent

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

## 接入自己的应用

```js
const api = "http://127.0.0.1:7311";
const res = await fetch(`${api}/v1/sessions?q=s:work`);
const seq = res.headers.get("x-uniflo-seq");          // 快照对应的序号
render(await res.json());

const es = new EventSource(`${api}/v1/stream?since=${seq}`);   // 从快照之后无缝续上
es.addEventListener("session", (m) => upsertSession(JSON.parse(m.data).session));
es.addEventListener("event", (m) => upsertEvent(JSON.parse(m.data).event)); // 同 id 覆盖 = 流式更新
```

完整接口见 [`docs/api.md`](docs/api.md)。

## 网页演示

[`examples/web/index.html`](examples/web/index.html) 是一个无外部运行时依赖、免构建的单文件客户端，守护进程直接在 `/demo` 提供，也可以复制出去改造：

顶栏五个视图（数字键 `1`–`5` 切换），视图、过滤、下钻和排序都写进 URL，刷新或分享链接原样恢复：

- **会话**：实时会话流，见下。会话头显示 API 等价成本、五类 token、最后一步的上下文占用；「每步用量」展开逐步明细；「恢复」复制恢复命令，macOS 上还可直接在 Terminal / iTerm2 / Ghostty 中打开。
- **用量**：24 小时 / 7 / 30 / 90 天或自定义区间，按 harness、模型、项目过滤；KPI、按 harness 或模型堆叠的每日费用图、可排序明细表（harness / 模型 / 项目 / 目录 / 日期分组，目录可逐级下钻，带面包屑），导出 CSV。费用一律标注「API 等价成本」，未定价的步骤单独计数并可展开到模型列表。
- **检索**：全文检索，结果按会话分组、命中词高亮；点击片段打开会话并滚动到该事件高亮。
- **管理**：勾选会话 → 生成清理计划（可释放空间、预计归档大小、不可清理的原因）→ 确认执行，逐项显示结果；「归档」页查看与删除归档。守护进程以 `--read-only` 启动时隐藏所有写操作并说明原因。
- **洞察**：最近一年的每日热力图、星期 × 小时分布、项目 / 模型 / harness 前 5 排行，可切换会话 / 提示 / token / 费用指标。

会话视图：

- 左侧：fd/fzf 式搜索、可搜索的 harness 单选下拉、按「工作中 / 最近」分组的会话列表（状态点、子代理、存活进程）。选择新的 harness 会替换原来的正向 `h:` / `harness:` 条件，保留其他查询条件；选「全部 harness」清除该条件。
- 中间：归一后的会话流——用户气泡、Markdown 回复（代码块可复制）、折叠的思考、一行一个的工具调用（参数 + 输出 + 耗时 + 状态），按回合汇总 token。
- 右侧：14 个接口的实时自检与延迟、索引健康度、实时 Envelope 流。
- 顶栏切换 SSE / WebSocket / NDJSON；跟随系统明暗主题；`/` 搜索，`j` `k` 切换会话。
- harness 显示单色品牌图标（[lobe-icons](https://github.com/lobehub/lobe-icons)，MIT，见 `THIRD_PARTY_NOTICES.md`），没有图标的显示字母块。
- 回合结束通知（铃铛按钮，默认关闭）：页面在后台时，会话从工作转为空闲会弹系统通知，标题是会话标题，同一会话 30 秒内最多一次，点击通知打开该会话。
- 响应式布局：宽屏三栏，≤1180px 自动收起右栏，≤760px 改为会话列表 / 会话内容单栏切换；「会话列表」按钮或 `Esc` 返回列表，`/` 打开搜索。通知与工具标题按内容撑高，不遮挡相邻行。
- URL 参数：`?api=http://127.0.0.1:7311`（从其他本地端口打开时）、`?token=`、`?transport=ws`、`?select=<key>`、`?redact`（模糊所有正文，便于录屏）。

harness 下拉使用 [Tom Select](https://tom-select.js.org/) 2.6.2：脚本、样式及 Apache-2.0 许可证已内嵌，不请求 CDN，也不需要 npm 安装或前端构建。触发器、弹层、选中项和焦点沿用页面主题 token，弹层支持内部滚动及短视口向上展开。

`scripts/demo-e2e.mjs` 用无头 Chrome 对它做端到端验收（合成数据，不碰真实会话）：三种传输、全部接口、实时追加延迟、work/idle 切换、跨域接入与 token；全部 harness 的图标、用量与 `/v1/usage` 逐项一致、检索跳转、洞察与分组接口一致、通知节流、清理（临时 `UNIFLO_TRASH_DIR`）与只读模式；再按 5 个视图 × 1480 / 390 px × 明暗主题截图，检查横向溢出、元素重叠和 WCAG AA 对比度。打开终端被页面内拦截，不会弹出真实窗口。

`/demo` 从 `crates/uniflo-gateway/src/index.html` 编译进守护进程；修改演示页时需同步该副本，再重新构建并重启守护进程、刷新浏览器。

![demo light](docs/assets/demo-light.png)

## 项目结构

```text
crates/
  uniflo-schema    wire 类型（唯一对外契约）
  uniflo-core      适配器 trait、JSONL 驱动、状态机、引擎、缓存、监听、进程探测、用量账本与价格目录
  uniflo-search    查询语法 + 模糊排序；全文索引（SQLite FTS5）
  uniflo-adapters  每个 harness 一个模块 / 一个 cargo feature
  uniflo-gateway   REST / SSE / NDJSON / WebSocket + Host/Origin/token 守卫
  uniflo-cli       uniflo 二进制：守护进程、查询客户端、MCP 服务器、setup
examples/web       网页演示（同时由守护进程在 /demo 提供）
docs/              架构、契约、规范、决策记录
```

设计说明见 [`docs/architecture.md`](docs/architecture.md)，决策见 [`docs/decisions/`](docs/decisions/)。

## 开发

```sh
scripts/verify.sh          # fmt + clippy -D warnings + 每个适配器 feature 单独编译 + 全量测试
scripts/verify.sh --e2e    # 再用无头 Chrome 跑一遍网页演示（需要 bun + Chrome）
```

开发规范：[`docs/conventions/DEVELOPMENT.md`](docs/conventions/DEVELOPMENT.md)；agent 协作规范：[`AGENTS.md`](AGENTS.md)。新增 harness：[`docs/adapters.md`](docs/adapters.md)。

## 许可

MIT
