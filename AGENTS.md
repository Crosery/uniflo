# AGENTS.md · Uniflo

Uniflo：本机常驻守护进程，持续读取所有 agent harness（Claude Code、Codex、omp、OpenCode…）的会话，归一成一套 wire schema，通过本地网关（REST / SSE / NDJSON / WebSocket）给桌面端、网页端实时消费。人读总览看 `README.md`；本文只放会改变 agent 行为的规则与指针，规范正文全在 `docs/`。

本仓只有这一个 agent 入口：Claude Code、Codex、omp、dsh 都直接读 `AGENTS.md`。技能单一实现放 `.agents/skills/`，`.claude/skills/<name>` 只是指向它的符号链接。

## 按任务读

| 任务 | 读 |
|---|---|
| 任何改动 | `docs/README.md`（docs 地图）→ 本表对应行 |
| 改代码（任何 crate） | `docs/conventions/DEVELOPMENT.md`、`docs/architecture.md` |
| 改 wire schema（`uniflo-schema`） | `docs/schema.md` + `docs/decisions/ADR-0003-wire-schema-只增不改.md` |
| 新增或修改 harness 适配 | `docs/adapters.md` |
| 改网关接口、鉴权、CORS | `docs/api.md` + `docs/decisions/ADR-0004-网关只监听本机.md` |
| 改搜索语法 | `docs/search.md` |
| 改网页演示 | `examples/web/index.html`、`scripts/demo-e2e.mjs`、`docs/conventions/DEVELOPMENT.md#测试` |
| 提交代码 | `docs/conventions/COMMITS.md` |
| 分支、MR/PR、合并 | `docs/conventions/BRANCHING.md`、`docs/conventions/GIT.md` |
| 写或改文档、ADR、worklog | `docs/conventions/DOCUMENTATION.md` |
| 行为变更、缺陷、测试、交付证据 | `docs/conventions/TESTING.md` |
| 较大需求（跨模块、要澄清、要分阶段验收） | 用 `/comet` 走 Native 流程；规范文档已接入它的项目知识检索 |
| 为什么这样定 | `docs/decisions/ADR-*.md`；过程与证据 `notes/` |

## 契约

- **wire schema 真源是 `crates/uniflo-schema/src/lib.rs`**，文档是 `docs/schema.md`。v1 只增不改：只能加可选字段和新 `kind`；改名、删字段、改语义要升 `SCHEMA_VERSION` 并写 ADR。改 schema 必须在同一提交同步 `docs/schema.md`。
- **依赖方向单向**：`schema ← core ← search / adapters ← gateway ← cli`。下游不得被上游引用；`uniflo-schema` 只依赖 serde。新 crate 或新边要写进 `docs/architecture.md`。
- **适配器只翻译，不判状态**：work/idle 由 `uniflo-core::status` 从归一事件统一推导。适配器不得自己写状态机；只能通过 `TurnStart`/`TurnEnd` 事件或 `live()` 表达。
- **只读 harness 数据**：任何适配器、测试、脚本都不得写入、移动、复制、锁定 harness 的会话文件或数据库。SQLite 一律走 `sqlite::open_ro`（`SQLITE_OPEN_READ_ONLY`）。
- **隐私**：测试夹具只能手写合成数据，禁止拷贝真实会话；调试输出只打印键名、计数、类型，不打印会话正文、提示词、文件内容、token。`~/.claude/sessions/*.key` 之类的凭据文件永不读取。
- **网关默认只听回环**：非回环 Host 必须同时配 `--allow-host` 和 token；不得放宽 Host/Origin 校验来"方便调试"。
- 生成物不入库：`target/`、缓存（`~/Library/Caches/uniflo/`）不提交。

## 非显然的环境事实

- 默认端口 `127.0.0.1:7311`；CLI 读 `UNIFLO_URL`、`UNIFLO_TOKEN`；`UNIFLO_HOME` 覆盖家目录（测试用）。
- 索引缓存 `~/Library/Caches/uniflo/index-v1.json`（macOS），标签含版本、schema 版本和适配器列表，任一变化自动失效；排查索引问题时加 `--no-cache`。
- 全文索引 `~/Library/Caches/uniflo/fts-v1.sqlite`（含 `-wal`/`-shm`，本机约 5.6 GB，首建约 14 min），标签含格式版本、Uniflo 版本和 schema 版本，变化即整份重建。缓存目录随 `UNIFLO_HOME` 移动；一个缓存目录同时只该有一个守护进程写索引；用真实家目录另起守护进程做实验时，若常驻守护进程也开着全文索引，给其中一个加 `--no-fts`。
- macOS FSEvents 报告规范化路径（`/private/var/...`），`core::watch` 会映射回配置的根；新增根目录时别绕过它。
- omp/Pi 没有在线注册表：存活进程靠 `core::procs`（`ps` + 一次批量 `lsof`）按 `--resume` 参数 → 打开的会话文件 → cwd 推断三级映射。Codex app-server 一个进程服务多会话，不做 pid 映射，状态全靠 `task_started/task_complete`。
- Claude 子代理收尾时 `stop_reason` 为空、没有回合结束标记，靠 90 s settle 窗口转 idle；不要为此给适配器加时间判断。
- 本仓 Rust edition 2024 写在各 crate 的 `Cargo.toml` 里（不用 `edition.workspace`），因为本机 rustfmt 钩子靠它识别 let-chains。

## 完成标准

行为改动先写可观察的完成条件；交付前跑 `scripts/verify.sh`（fmt、clippy `-D warnings`、各适配器 feature 单独编译、全量测试；动了网关或网页演示加 `--e2e`）并把命令与真实结果写进当天 worklog。改适配器还要对本机真实数据跑 `uniflo scan --no-cache --json`，确认 `stats.unknown` 为空、`bad_lines` 与 `read_errors` 为 0（只看计数，不打印正文）。没跑的检查在回复里标「未验证」。
