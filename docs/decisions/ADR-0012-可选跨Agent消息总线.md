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
2. **严格保持 harness 会话数据只读**：
   - 消息绝不写入任何 harness 的 transcript 文件或数据库。
   - 工作中会话由各 harness 原生 Hook（`uniflo message hook`）在工具/回合边界毫秒拉取未投递消息并输出 `<uniflo-messages>` 注入上下文，无未读时完全静默零输出；存活空闲或离线会话可由外部编排器（如 Herdr）订阅 `GET /v1/messages/stream` SSE 实时流，或按响应自带的 `resume_command` 显式唤醒。
3. **网关路由与写守卫（`uniflo-gateway::message`）**：
   - 读接口：`GET /v1/messages`（查收件箱）、`GET /v1/messages/stream`（SSE 实时推送）。
   - 写接口：`POST /v1/messages`（发信）、`POST /v1/messages/ack`（标记已投递/已读）；全部自动经 `uniflo-gateway::write::check` 校验回环 Host/Origin、`X-Uniflo-Write: 1` 与可选 token，`--read-only` 时一律 403。
4. **`uniflo setup` 默认关闭，显式按需启用（扩展 ADR-0007）**：
   - 默认运行 `uniflo setup`（含 `--mcp` / `--skill` / `--both`）**不启用**消息功能：MCP 条目仍注册纯只读的 `<uniflo> mcp`（仅 10 个 `readOnlyHint: true` 工具），不写任何消息 Hook。
   - 仅当用户显式传 `--message` 或在交互提问（默认「否」）/ 升级新功能提示中确认开启时：
     - **全 MCP 进程级 SSE 看门狗（21 个 MCP harness 通用）**：MCP 条目注册 `<uniflo> mcp --message`，每个 agent 启动时都会绑定一个随进程同生共死的轻量 SSE 看门狗线程（直连 `/v1/messages/stream?cwd=…`，零轮询、不扫盘、不争抢索引锁），实时推送 JSON-RPC `notifications/message` 与 `uniflo://inbox` 资源更新，并在工具返回尾部自动顺带投递未读消息；额外暴露 4 个工具：`uniflo_send_message`、`uniflo_inbox`、`uniflo_ack_messages`、`uniflo_wait_message`（阻塞等待回复，毫秒级唤醒）。
     - **全量原生静默拉取 Hook（10 个支持声明式 Hook 的 harness）**：按各家真实格式自动写入 `uniflo message hook --harness <id> 2>/dev/null || true`，覆盖 `claude`、`codex`（`~/.codex/hooks.json`）、`gemini`（`~/.gemini/settings.json`）、`antigravity`（`~/.gemini/config/hooks.json` 的独立 `uniflo` 命名空间）、`codebuddy`、`qoder`、`workbuddy`、`commandcode`、`reasonix`、`devin`（改前备份、原子写、不碰用户自有条目）。
     - **Skill 兜底（剩余 12 个无 MCP / 无 Hook harness）**：内置 `SKILL.md` 附带 `uniflo message send|inbox|ack|watch` 规范。
   - 写入状态记录在 `<配置目录>/setup.json`（`message`、`message_hooks`、`seen_features`），`uniflo setup --uninstall` 对称清理，`--reload` 与升级后的 `--after-update` 自动刷新已有条目并补接新检测到的 harness，仅在出现尚未见过的重大新功能时才询问一次。

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
