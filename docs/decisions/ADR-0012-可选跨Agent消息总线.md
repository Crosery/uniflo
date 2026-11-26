# ADR-0012 · 可选跨 Agent 消息总线与按需接入

> 在单一常驻 Uniflo 守护进程内提供懒初始化的本地消息信箱与 SSE 实时推流，供多个本地 Agent 会话低延迟互通；对所有 harness 会话文件仍严格只读，`uniflo setup` 默认关闭该能力，仅在用户显式开启（`--message` 或交互选择）时写入带 `--message` 的 MCP 条目与可选拉取 Hook，并记入 `setup.json` 对称撤销。

状态：`accepted` · 更新：2026-11-26

## 背景

用户常在同一台机器上同时运行多个 Agent（Claude Code、Codex、Antigravity 等），希望一个 Agent 能像 `claude message` 一样低延迟地给另一个正在工作或空闲的 Agent 发消息，而不必手工切窗口复制粘贴。

Uniflo 已经以单守护进程维护全机所有会话的实时索引（`key`、`harness`、`cwd`、`status`、`pid`、`resume` 恢复命令），并已将网关并发读优化到毫秒级。如果每个 Agent 各自再挂一个后台轮询看门狗进程，不仅会放大 CPU/内存开销、争抢全文索引写租约，也无法突破「不能直接改写 harness 会话文件」的只读契约。同时，并非所有 Uniflo 用户都需要跨 Agent 发信能力，默认安装的 `uniflo mcp` 应保持纯只读零侵入。

## 决策

1. **单共享守护进程 + 懒初始化消息总线（`uniflo-core::message::MessageBus`）**：
   - 守护进程持有内存 `RwLock` 索引 + `tokio::sync::broadcast` 实时推送通道，持久化使用追加写 NDJSON 日志 `<数据目录>/messages/log.jsonl`（最多保留最近 10,000 条，启动重放并在冗余翻倍时自动压缩），不新增外部依赖。
   - **零开销懒创建**：只要从未发送过消息，不创建 `messages/` 目录和日志文件。
   - 收件目标支持精确会话 key、唯一 key/id 前缀、或 `cwd:<目录>` / 目录路径（优先命中该目录下 `work` 会话，其次存活 `idle` 会话，再其次最近会话），并返回按目标当前状态判定的投递提示（`hook` / `pty_or_hook` / `resume`）及恢复命令。
2. **零 Hook 主动投递引擎（`uniflo-core::deliver`）+ 严格保持 harness 会话数据只读**：
   - 消息绝不直接改写任何 harness 的 transcript 文件或数据库。
   - 当发信请求携带 `deliver: "auto" | "wake"` 且未达到服务端防循环跳数上限（`MAX_AUTO_WAKE_HOPS = 2`）时，无需安装任何 Hook 即可打通空闲与在线会话的真实对话：
     1. **Herdr 活终端直推**：通过 `session.pid` 进程树反查（或唯一 `cwd + harness` 匹配）定位目标活终端，先检屏避开权限确认弹窗，再经 `herdr-core send` 毫秒级注入活终端输入区；
     2. **tmux 活窗格直推**：按 `session.pid` 匹配 `tmux list-panes` 窗格，检屏安全后通过 `tmux send-keys` 注入；
     3. **官方无头原上下文续跑**：目标不在活终端（或窗口已关）时，用各家官方校验过的无头命令（`agy -r=<id> -p=…`、`claude -p --resume <id>`、`codex exec resume <id>`、`opencode run -s <id>`、`omp -p -r <id>`、`pi -p --session <id>`、`gemini -r <id> -p …` 等，严格经 `resume::valid_id` 防参数注入）在原会话上下文唤醒续跑；回复自动写回同一会话文件并记录到消息总线。
   - 若调用方传入 `wait_secs`，网关在投递前即同时订阅消息总线与 `Engine` 实时事件流，对方无论回复在消息总线还是直接输出到会话正文（`AssistantMessage` + `TurnEnd`），都会自动捕获并内联返回给发信方（实现完全零 Hook 的单次调用问答）。
   - `uniflo mcp --message` 进程启动时按环境变量与祖先 PID（`/v1/messages/peers?pid=…`）自动识别自身 `self_key`，发信自动补 `from`、按目录寻址自动排除发信方自身、看门狗精确过滤自身消息与已发 ID，彻底杜绝同目录（如 `~`）多会话互相截胡 `delivered_at`。
   - 原生 Hook（`uniflo message hook`）降级为可选增强：仅用于在工作中的会话工具边界顺带注入消息，不再是跨 Agent 对话的必要前提。
3. **网关路由与写守卫（`uniflo-gateway::message`）**：
   - 读接口：`GET /v1/messages`（支持 `peer` 双向查询）、`GET /v1/messages/peers`（按 `pid` 反查 `self_key` 并返回同事可达通道）、`GET /v1/messages/stream`（SSE 实时推送）。
   - 写接口：`POST /v1/messages`（发信 + 可选零 Hook 唤醒）、`POST /v1/messages/ack`（标记已投递/已读）；全部自动经 `uniflo-gateway::write::check` 校验回环 Host/Origin、`X-Uniflo-Write: 1` 与可选 token，`--read-only` 时一律 403。
4. **`uniflo setup` 默认关闭，显式按需启用（扩展 ADR-0007）**：
   - 默认运行 `uniflo setup`（含 `--mcp` / `--skill` / `--both`）**不启用**消息功能：MCP 条目仍注册纯只读的 `<uniflo> mcp`（仅 10 个 `readOnlyHint: true` 工具），不写任何消息 Hook。
   - 仅当用户显式传 `--message` 或在交互提问（默认「否」）/ 升级新功能提示中确认开启时：
     - **全 MCP 进程级 SSE 看门狗 + 一键同事发现与对话（21 个 MCP harness 通用）**：MCP 条目注册 `<uniflo> mcp --message`，额外暴露 5 个工具：`uniflo_peers`、`uniflo_send_message`（默认 `wake: true`，支持 `wait_secs` 一键发信唤醒并收回执）、`uniflo_inbox`、`uniflo_ack_messages`、`uniflo_wait_message`（共 15 工具）。
     - **可选原生静默拉取 Hook（10 个支持声明式 Hook 的 harness）**：按各家真实格式自动写入 `uniflo message hook --harness <id> 2>/dev/null || true`（改前备份、原子写、对称撤销）。
     - **Skill 兜底（剩余 12 个无 MCP / 无 Hook harness）**：内置 `SKILL.md` 附带 `uniflo message peers|send|inbox|ack|watch` 规范。
   - 网页控制台 `/demo` 在非只读、未归档会话的「对话」页底部提供参照 Herdr Pocket 风格的悬浮输入卡，支持直接发消息、实时切换「自动唤醒 / 仅收件箱」并即时展示投递通道与回复流。

## 否决项

- **每个 Agent 启动一个独立的 Uniflo 后台轮询看门狗**：重复扫盘、内存成倍增长、争抢 `fts-v2.sqlite.writer` 租约；单守护进程 + SSE 推流 + Hook 按需拉取延迟更低且零轮询浪费。
- **直接往 harness 的 JSONL / SQLite 会话文件注入消息**：违背 `AGENTS.md` 只读契约，且与运行中的 harness 并发写极易损坏会话文件。
- **在 `uniflo setup` 中默认开启消息工具与 Hook**：会改变现有纯只读 MCP 心智，并给不需要 Agent 间通信的用户增加额外工具与 Hook 钩子。
- **引入独立 SQLite 库存储消息**：`uniflo-core` 不依赖 `rusqlite`，追加写 NDJSON + 内存 `RwLock` 读写在微秒级，重启重放简单且零新依赖。

## 后果

- 收益：开启后任意接入 MCP 的 Agent 可在毫秒级发现同伴并跨 harness 发信，工作中的 Claude Code 会话在工具/回合边界自动无感接收；未开启用户行为、工具列表、配置文件和磁盘状态完全不变。
- 代价：`setup` 对 `is_ours` / `toml_entry` 的识别需同时接受 `["mcp"]` 与 `["mcp", "--message"]`；消息 Hook 首批覆盖 Claude Code 原生 JSON Hook 配置，其他 harness 可通过 `uniflo message hook` / `uniflo message watch` 或 `uniflo mcp --message` 接入。
- 同步：`docs/api.md`（含 `#写接口`）、`docs/schema.md`、`docs/agents.md`、`docs/architecture.md`、`docs/README.md` 及网页控制台 `examples/web/index.html` / `crates/uniflo-gateway/src/index.html` 同步登记新接口。

## 复议触发

- 更多 harness（Codex、Gemini CLI、Antigravity 等）稳定提供统一声明式 Hook 配置文件格式时，扩展 `setup` 的可选消息 Hook 覆盖表。
- 单机消息量或检索需求超出内存 + NDJSON 轻量日志定位时，再评估是否单独落 SQLite 或对接外部编排器。
