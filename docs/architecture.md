# 架构

> 守护进程如何从各 harness 的会话存储得到统一、实时的会话与事件流。

状态：`current` · 更新：2026-10-10

## 数据流

```text
harness 存储（JSONL / JSON / SQLite）
   │  FSEvents/inotify 通知 · 200 ms 热轮询 · 30 s 兜底重扫
   ▼
Adapter::read(source, cursor)        ← uniflo-adapters（每个 harness 一个模块）
   │  Record::Meta / Record::Event（已归一）
   ▼
Engine（单写者循环）                  ← uniflo-core
   ├─ StatusTracker：事件序列 → work / idle
   ├─ live()：存活进程 → pid、退出即 idle
   ├─ 会话表 + 每源游标（字节偏移 / rowid）
   └─ 广播 + 重放环（8192 条 Envelope，带全局 seq）
   │       └─► Fts 写线程               ← uniflo-search::fts（广播 + history 分页 → fts-v1.sqlite）
   ▼
Gateway（axum）                       ← uniflo-gateway
   REST 快照 · SSE / NDJSON / WebSocket 实时流 · /v1/search（只读 FTS 索引）
   写接口（守卫对非 GET/HEAD/OPTIONS 调 write::check）：/v1/cleanup/*、DELETE /v1/archive/{key} → uniflo-core::cleanup；open-terminal → uniflo-core::resume
   ▼
桌面端 / 网页端 / CLI（uniflo ls / tail / watch）
```

## 读取模型

- **一个源 = 一个文件或一个数据库**，源里可以有多个会话（SQLite）或一个会话（JSONL）。
- **冷启动只读头尾**：JSONL 源读开头 256 KB（元数据）+ 末尾 256 KB（最近状态），不解析全文；之后按字节偏移跟随。大文件历史通过反向逐行读取按 `before=pos` 分页。
- **跟随**：偏移之后的新字节逐行解码；半行留到下次；文件变短或被重写视为重置，重新做头尾摘要。一次追平上限 16 MB，超过改走摘要。
- **SQLite**：只读打开，按 rowid 读新行，未完成的 part/message 按主键重读；WAL 签名变化才触发读取。
- **同一 `id` 的事件是覆盖**（流式更新）：客户端按 `(session, id)` upsert。
- **流式片段**：`LineDecoder::finish` 在一次连续读取窗口结束时被调用，把还没闭合的消息（如 Grok 按片段写的文本）以 `partial` 发出；后续读到完整消息时复用同一 id 覆盖。历史分页只从 `LineDecoder::page_start` 认可的行开始（Grok 是带回合号与模型的提问、Kimi 是 `turn.prompt`），页内按 `pos` 排序，所以任意 `limit` 下翻页拿到的 id 与正文都和整文件解码一致；页尾之后还有内容时，未闭合的消息按完整发出。
- **旁路文件**：会话目录里原地重写的 JSON（标题、cwd、模型、父会话、逐回合用量）由 `uniflo-adapters::common::WithSidecars` 包装的 `Sidecar` 读取：摘要 / 重置、会话首次出现内容、或旁路文件签名（mtime + 大小）变化时才补发；只有元数据、没有任何事件的会话不列出（grok、kiro、kimi）。
- **整文件重写**：Craft 以「临时文件 → 删除 → 改名」重写 `session.jsonl`，所以源是会话目录，每次读取整源重读并 `reset`；文件短暂缺失时保留上次结果，不计读错误。
- **库内会话树**：Devin、OpenClaw 的消息是一棵树，只展示当前可见分支；分支延长按行跟随，切换到别的分支时整库重读（`reset`）。OpenClaw 库里的条目是 Pi 条目，经 `uniflo_core::decode_record` 交给 Pi 的 `LineDecoder` 解码，不重复实现。
- **Cursor IDE 库**：`state.vscdb` 可达 GB 级，只按键前缀区间枚举、按主键取单值，不对每个值 `json_extract`；结果按库文件与 WAL 的签名缓存，签名不变不重读。
- **缓存**：索引完成后、每 30 s（有变化时）和退出时把会话快照和游标写到 `~/Library/Caches/uniflo/index-v1.json`；启动时未变的源直接恢复，只读新增字节。缓存目录由 `core::cache::dir()` 给出，设了 `UNIFLO_HOME` 时随之移到其下，测试不会碰到真实缓存。

## 全文索引

决策与取舍见 `docs/decisions/ADR-0008-全文检索索引.md`，这里只写运行时结构。

- **单写者**：`Fts::start` 起一个 `uniflo-fts` 线程，独占 `fts-v1.sqlite` 的写连接；查询走只读连接池，WAL 下读写互不阻塞。守护进程启动时先就绪 REST，索引在后台追赶。
- **数据只来自引擎**：正文通过 `Engine::history`（与 `GET …/events` 同一条读取路径）分页读取，新事件来自引擎广播；不直接打开 harness 文件。
- **一次 pass**：一个会话从最新一页往回读，读完记下 `done_upd` / `done_pos`；会话按 `updated_at` 倒序排队。广播丢失（lagged）、摘要读取不广播的事件、被删除的会话，由每 30 s 一次的对账补齐。
- **pinned 会话**：`Fts::index_session` 写入的会话不在引擎里，对账和 `removed` 不删除它们，只有 `remove_session` 删除。
- 依赖：`uniflo-search` 因此依赖 `uniflo-core`（读 `Engine`）和 bundled `rusqlite`；网关只持有 `Option<Arc<Fts>>`，为 `None` 时 `/v1/search` 返回 503。

## 用量与费用

头尾摘要不够算全量用量，所以用量走一条独立的后台链路（`uniflo-core::usage`、`uniflo-core::pricing`，引擎胶水在 `engine_usage.rs`）：

```text
index()（头尾摘要，/v1/health 可用）
   ▼
run() → usage_loop
   ├─ 恢复 usage-v1.json（与索引缓存同目录，标签含版本与 LEDGER_VERSION）→ 只跟随之后追加的部分
   ├─ 其余源排队，按最近活动倒序，在 threads/2（1–8）个线程上 Adapter::read_all 全量读
   ├─ 引擎每应用一次源变化 → 从账本自己的游标 Adapter::read 跟随（与引擎游标互不影响）
   ├─ 每秒检查价格文件变化 → 重新计价全部步骤；守护进程按 PriceSync 定时同步价格
   └─ 首次读完、之后每 5 分钟（有变化时）、退出时写回 usage-v1.json
   ▼
每会话账本 Ledger：步骤（usage 事件，按事件 id 去重 / 覆盖）+ 提问（非注入 user_message）+ 回合
   ▼
Session.usage（随 session envelope 推送）· /v1/usage 聚合 · /v1/sessions/{key}/usage · /v1/models
```

- **回合**：非注入的 `user_message` 或 `turn_start` 开一个新回合，中间没有任何活动的相邻边界合并成一个。
- **步骤模型**：事件自带 `model` 优先，否则取该会话最近一条 assistant 消息 / 元数据的模型。
- **价格三层**：`<数据目录>/pricing/overrides.json`（用户覆盖，同步不碰）> `catalog.json`（同步结果）> 内置快照 `crates/uniflo-core/data/pricing-snapshot.json`。按事件时间选价格段，所以重建索引后费用不变。规则见 ADR-0010。
- **全量读**：`Adapter::read_all` 默认先取摘要元数据，再逐会话 `history()` 全量重放；JSONL 适配器按 4 MiB 分块顺序解码（单行更长时窗口翻倍），Hermes 覆盖它以追加会话级 usage。
- **代价**（本机约 17 GB 会话数据、38 万步）：冷启动后台全量读约 12 s，缓存 36 MB，常驻内存比不建用量索引时多约 130 MB；有缓存时启动到 `/v1/health` 与改动前持平，账本恢复约 1 s。

## 会话清理与归档

用户确认后把会话源文件移入系统回收站，先写精简归档；决策与安全边界见 `docs/decisions/ADR-0006-会话清理与写接口.md`，接口见 `docs/api.md#会话清理`。代码在 `uniflo-core::cleanup`（计划与执行、`trash`、`targets`）、`uniflo-core::archive`（归档存储、墓碑、`compact` 精简与 zstd 编解码），引擎胶水在 `engine_archive.rs`。

```text
POST /v1/cleanup/plan ─► Cleanup::plan：Engine::cleanup_view() + Adapter::cleanup_targets → 判定、文件戳（只读）
POST …/execute ─► Cleanup::execute（同一时间只跑一个），每个会话：
   ├─ 重新判定、比对文件戳 → 读文件算 SHA-256
   ├─ Engine::archive_material（全量读 + 去重 + 补 usage 模型）→ compact → archive/<harness>/<id>.jsonl.zst
   ├─ 清单写 cleanup.log.jsonl（fsync）+ index.json → 再比一次文件戳
   ├─ Engine::retire：先在内存立墓碑，再把源换成归档条目、用量账本换成归档账本（广播 session，archived: true）
   └─ Trash::trash 逐个目标 → 持久化 tombstones.json；第一个就失败则 unretire 并撤销归档
```

- **归档会话**：`Entry.archive` 指向归档文件；`history()` 从归档解码分页，摘要、全文索引、用量都走这一条，全文索引不会因为源没了而删掉它（引擎不对归档会话发 `removed`）。启动时 `index()` 末尾 `load_archived` 把没有源的归档条目放回会话表；用量账本由归档重建，键是归档路径。
- **墓碑**：`apply()` 丢弃墓碑下的源的读取结果（防止清理进行中的读取把会话写回）；`index()` / 新源接纳前调 `ArchiveStore::admit`：内容与清单一致（大小 + SHA-256）才撤销墓碑并作为源读取，不一致的按 (大小, mtime) 记住、不再重复哈希。文件监听命中墓碑下的路径时立即重扫，所以从回收站还原的源通常 1 s 内出现。
- **还原**：源重新出现时 `apply()` 用源替换归档条目、移除归档用量账本（`unarchive`），不会重复计数；归档文件和 `index.json` 条目保留到 `DELETE /v1/archive/{key}`。
- **回收站**：`Trash` trait；`SystemTrash`（`trash` crate，macOS 走 `NSFileManager`）、`DirTrash`（测试、`UNIFLO_TRASH_DIR`）、`NoTrash`（设了 `UNIFLO_HOME` 而没设 `UNIFLO_TRASH_DIR`）。
- 依赖：`uniflo-core` 新增 `sha2`、`trash`，zstd 复用已有的 `ruzstd`；网关只持有 `Option<Arc<Cleanup>>`，为 `None` 时清理接口返回 503。

## agent 接入

用法与边界见 `docs/agents.md`、`docs/decisions/ADR-0007-setup-写-harness-配置.md`，这里只写代码分布与数据路径。

```text
agent（Claude Code、Codex…）──stdio JSON-RPC──► uniflo mcp（uniflo-cli::mcp）
   │  每个工具 = 一个 GET
   ├─ 守护进程在：HTTP 到 UNIFLO_URL（与 CLI 同一个 client）
   └─ 不在：进程内 Engine::index() + uniflo_gateway::router()，agent::get_in_process 用 tower oneshot 直接调路由
   ▼
网关路由（REST 同一段代码）→ structuredContent
```

- `uniflo-core`：`resume`（各 harness 的恢复命令、会话 id 校验、POSIX / PowerShell 转义、终端启动脚本的 argv）、`memory`（记忆与指令文件的列举与无状态读权限判断）、`context`（项目最近会话的筛选与 Markdown）。都是纯函数，不碰网络。
- `uniflo-gateway::agent`：`/v1/sessions/{key}/resume`、`/v1/sessions/{key}/open-terminal`（写接口，`osascript` 经可注入的 `Launcher` 运行，测试不开窗口）、`/v1/memory`、`/v1/memory/file`，以及 `get_in_process`。为此网关依赖 `tower`（`util` 特性，已在 axum 的依赖树里）。
- `uniflo-cli`：`mcp`（协议层与工具）、`agent`（`resume` / `context` / `skill` 命令，`skill/SKILL.md` 编进二进制）、`setup/`（`harness.rs` 检测表、`edit.rs` JSON / TOML / hook 编辑与原子写、`mod.rs` 流程与 `setup.json` 记录）。setup 是安装器逻辑，只属于二进制，不进库 crate。

## 自有目录

`uniflo-core::paths` 给出 Uniflo 自己的目录（不是 harness 的）：

| 函数 | macOS 默认 | 覆盖 |
|---|---|---|
| `data_dir()` | `~/Library/Application Support/uniflo`（`pricing/`、会话清理的 `archive/` 在这里） | `UNIFLO_DATA_DIR` |
| `config_dir()` | `~/Library/Application Support/uniflo`（`setup.json`、安装脚本写的 `install.json` 在这里） | `UNIFLO_CONFIG_DIR` |
| `cache_dir()` | `~/Library/Caches/uniflo` | `UNIFLO_CACHE_DIR` |

设了 `UNIFLO_HOME` 时，三者按相对真实家目录的同一路径改挂到它下面，测试不会碰到真实目录。

## 分发与升级

决策见 `docs/decisions/ADR-0009-预编译分发与自升级.md`，这里只写代码分布。

- `uniflo-core::install`：
  - 安装方式识别（`detect`：`install.json` → binary，cargo bin 目录 → cargo，其余 unknown）；
  - 发布包命名（`asset_name`、`TARGETS`）；
  - 自升级 `upgrade_binary`：系统 `curl` 下载 → `sha2` 校验 → 系统 `tar` / PowerShell `Expand-Archive` 解包 → `--version` 自检 → `swap` 改名替换（Windows 为 `.old` 改名）。
- `uniflo-core::update`：crates.io 版本检查（ADR-0005）。
- `uniflo-cli::update`：`uniflo update` 的接线：按安装方式分派、unknown 时的提示、launchd 重启判断，最后调用 `setup::after_update`。
- 脚本：
  - `scripts/package.sh`：打包，CI 与测试共用；
  - `scripts/install.sh` / `install.ps1`：安装；
  - `scripts/check-dist.mjs`：检查工作流、binstall 元数据与包名一致；
  - `scripts/third-party-notices.mjs`：生成第三方许可证清单；
  - `scripts/dist-e2e.sh`：本机发布包全链路。

## 状态机

适配器只产出事件，状态统一在 `uniflo-core::status` 推导：

| 事件 | 状态 |
|---|---|
| user_message、reasoning、assistant_message、tool_call、tool_result、turn_start、partial | `work` |
| turn_end | `idle` |
| usage、system | 不变 |

叠加规则（`effective()`）：

1. harness 给出明确在线状态（`LiveSession.status`）时以它为准。
2. 有存活进程（`pid`）时不判超时；进程消失而状态是 `work` → 强制 `idle`，原因 `exited`。
3. 无进程、`work` 且静默超过 10 min → `idle`（`stale`）；最后活动是助手文本时窗口缩到 90 s（`settle`，覆盖不写回合结束标记的子代理）。

## 存活探测

| harness | 来源 |
|---|---|
| Claude Code | `~/.claude/sessions/<pid>.json` 注册表，只用来判断 pid 是否存活（其中的 `status` 会过期，不采信） |
| WorkBuddy / CodeBuddy | `~/.workbuddy/sessions/<pid>.json` / `~/.codebuddy/sessions/<pid>.json`（同一内核） |
| omp / Pi | `core::procs`：`ps` 取启动时间与参数 + 一次批量 `lsof` 取 cwd 和打开的 `.jsonl`；映射优先级 `--resume` 参数 → 进程持有的会话文件 → 同 cwd 自启动以来写过的最新会话 |
| 其他 | 无进程映射，靠回合事件 + 超时 |

探测在 `spawn_blocking` 里跑，结果以消息形式回到引擎循环写入，循环本身不被 `ps`/`lsof` 阻塞。

## 并发与一致性

- 引擎状态 `RwLock<State>`，网关读快照不经过循环，读延迟与写入解耦。
- 所有写入（文件读取结果、存活探测结果、超时）在单个循环里串行应用，保证同一会话的 `Session` 快照按 seq 单调。
- 每个 `Envelope` 带全局递增 `seq`；客户端断线重连带 `since=<seq>`，重放环内的缺口补齐，超出则先收到 `lagged`，应改用 REST 重新同步。

## 扩展点

- 新 harness：实现 `LineDecoder`（一文件一会话的 JSONL）或 `Adapter`（其他），见 `docs/adapters.md`。
- 新传输：基于 `uniflo_gateway::stream::envelopes()`，与 SSE/NDJSON/WS 共用过滤与截断。
- 新写接口：用 `POST` / `DELETE` 等写方法挂到 `router_with`，网关守卫自动对它调用 `uniflo_gateway::write::check`；在 `docs/api.md#写接口` 登记。
- harness 图标：`uniflo-gateway::icons` 从内嵌的演示页里切出 `#harness-icons` 精灵图的 `<symbol>`，提供 `/v1/harnesses/{id}/icon.svg` 并填 `Harness.icon`；图标只有演示页这一份，换图标改页面即可。
- 网页客户端：`examples/web/index.html` 是参考实现（快照 + `since` 续流、按 id upsert、工具结果并入调用卡片）；`crates/uniflo-gateway/src/index.html` 是内容相同的 cargo 包内副本，由网关编译进 `/demo`，修改时必须同步。
- 嵌入式使用：直接依赖 `uniflo-core` + `uniflo-adapters`，`Engine::new(all(), opts)` → `index()` → `run()`，无需网关；不跑 `run()` 时用 `index_usage()` 一次性建好用量账本，再调 `usage_report()` / `session_usage()` / `models()`。`/v1/usage` 的参数解析与会话过滤在 `uniflo_gateway::usage::UsageParams`，CLI 本地模式复用它。要全文检索再加 `uniflo-search`，`Fts::start(engine, FtsOptions::default())` → `search()`。
