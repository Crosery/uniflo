# 开发规范

> 模块边界、代码风格、性能预算和隐私红线；改任何 crate 前读。

状态：`current` · 更新：2026-10-10

## 模块边界

| crate | 职责 | 允许依赖 |
|---|---|---|
| `uniflo-schema` | wire 类型（`Session`/`Event`/`Envelope`），唯一对外契约 | serde、serde_json |
| `uniflo-core` | 适配器 trait、JSONL 驱动、状态机、引擎、缓存、文件监听、进程探测、用量账本与价格目录（`usage/`、`pricing/`）、自有目录（`paths`）、会话清理与归档（`cleanup/`、`archive/`）、恢复命令 / 记忆文件 / 接力上下文（`resume`、`memory`、`context`）、安装方式识别与预编译包自升级（`install`） | schema |
| `uniflo-search` | 查询语法解析 + 模糊排序；全文索引 `fts`（SQLite FTS5，读 `Engine`） | schema、core |
| `uniflo-adapters` | 每个 harness 一个模块，每个模块一个 cargo feature | core、schema |
| `uniflo-gateway` | HTTP/SSE/NDJSON/WS 网关、Host/Origin/token 守卫，写请求另过 `write::check` | core、search、schema |
| `uniflo-cli` | `uniflo` 二进制：守护进程 + 查询客户端 + MCP 服务器（`mcp`）+ 接入安装器（`setup/`） | 以上全部 |

- 依赖只能向下指；`core` 不知道任何具体 harness，`adapters` 不知道网关。
- 一个 harness 格式 = 一个文件 + 一个 feature。共享逻辑放 `common.rs`（纯函数）或 `sqlite.rs`（只读 SQLite），不在适配器之间互相引用。
- 每个 feature 必须能单独编译且零警告：`scripts/verify.sh` 会逐个 `--no-default-features --features=<f>` 检查。
- 新增外部依赖属于需授权事项：先说明用途、体量和替代方案，用户同意后加到根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 里只写 `.workspace = true`。

## 代码风格

- Rust 2024，`rustfmt.toml`（宽 120）+ `cargo clippy --all-targets -- -D warnings` 零警告。
- 错误：库内 `anyhow::Result` 并 `.context()` 写清哪个路径；解析坏行计数（`bad_lines`），不 panic、不中断整个文件。
- 未识别的记录类型调用 `cx.unknown(...)` 计数，确认无用后加进模块的 `IGNORED` 列表，不能静默吞掉。
- 注释只写非显然的东西：格式怪癖、兼容处理、状态转换理由。不写流水账。
- 不在热路径分配大对象：跟随模式只读新增字节；SQLite 跟随按 rowid / 主键读，禁止全表扫描。

## 性能预算

| 路径 | 预算 | 怎么测 |
|---|---|---|
| REST 详情、事件分页、结构化过滤列表 | 单次 < 10 ms | `curl -w '%{time_total}'` 打本机守护进程 |
| 全量模糊搜索（fzf 语法扫全部会话） | < 50 ms | 同上，`/v1/sessions?q=<词>` |
| 追加一行到客户端收到事件 | < 50 ms（监听命中时个位数毫秒） | `crates/uniflo-core/tests/engine.rs`、`crates/uniflo-gateway/tests/gateway.rs` |
| 冷启动全量索引 | 本机全部会话 < 2 s；有缓存 < 300 ms | `uniflo scan --no-cache` / `uniflo scan` |
| 守护进程启动到 `/v1/health` | 用量索引不得拖慢：有缓存时不超过改动前 +50% | release 二进制 `uniflo daemon --bind 127.0.0.1:74xx`，隔离 `HOME` 与 `UNIFLO_DATA_DIR`，轮询 `/v1/health` |
| `/v1/usage` 聚合（索引就绪后） | 单次 < 100 ms（`project` 首次 stat 各 cwd 除外） | `curl -w '%{time_total}' '…/v1/usage?group_by=…'` |
| 追加 usage 到 `session` envelope 带新合计 | < 2 s（热会话通常 < 100 ms） | `crates/uniflo-gateway/tests/usage.rs` |
| 全文检索（`/v1/search`，≥ 3 字符的词，索引建成后） | p50 < 50 ms、p95 < 200 ms，代码子串 p50 < 50 ms；每个词的第一次（冷）查询计入 | 另起端口的守护进程在 `/v1/stats` 的 `fts.indexing`、`fts.warming` 都为 false 后，用这个进程里没查过的词集：按词频挑 10 个中文词 + 10 个代码子串，各查 5 次，至少 3 组，只记耗时与计数 |
| 含短词（< 3 字符）的全文检索：只有短词，或与 ≥ 3 字符的词混合 | 常见词 < 1 s；最多 2 s，超时返回 `partial` | 同上，按 `/v1/search` 实测；预算常量 `LIKE_BUDGET` |
| 新事件可被全文检索到 | < 2 s | `crates/uniflo-search/tests/fts.rs` |

引擎循环里不得做阻塞 IO：读文件、`ps`/`lsof`、SQLite 都走 `spawn_blocking`，结果回到单写者循环再写状态。

## 隐私与数据安全

- harness 数据只读；任何代码、测试、脚本不写、不移、不锁会话文件和数据库。例外只有两处：用户确认的会话清理（ADR-0006），只在 `uniflo-core::cleanup` 里，只移入回收站，测试注入回收站目录（`DirTrash` / `UNIFLO_TRASH_DIR`），绝不碰真实回收站，也不对真实数据执行清理；`uniflo setup` 写 harness 配置（ADR-0007），它的测试只在临时 HOME（`UNIFLO_HOME`）里执行写操作，真实家目录只跑 `--dry-run`。
- 测试夹具一律手写合成数据（见各适配器 `tests` 模块与 `common::testkit`）；不提交真实会话片段，哪怕脱敏。
- 调试真实数据只输出计数、键名、类型分布；日志里不打印会话正文。
- 凭据只走环境变量（`UNIFLO_TOKEN`）或钥匙串；不进源码、日志、产物。
- Uniflo 自己的文件只写到 `uniflo_core::paths` 给出的目录（数据、配置、缓存）；测试用 `UNIFLO_HOME` / `UNIFLO_DATA_DIR` 指到临时目录。
- 联网只用系统 `curl`，且只在规范写明的地方（更新检查、价格同步），都可关闭（`--no-update-check`、`--no-price-sync`），请求里不带任何会话数据。测试用本地 HTTP 服务（`UNIFLO_PRICING_URL`），不访问互联网。用户显式运行 `uniflo update` 时，binary 安装会下载发布包和 `SHA256SUMS`（ADR-0009）；测试用 `UNIFLO_RELEASE_BASE_URL`、`UNIFLO_UPDATE_INDEX_URL` 指向本地服务，安装目录与配置目录都用临时目录，不替换真实安装，也不重启真实的 launchd 服务。

## 测试

- 适配器：每个模块至少覆盖一个完整回合（用户 → 工具调用 → 工具结果 → 回复 → 结束）的事件序列、状态、元数据，以及 `unknown` 为空。
- 引擎与网关：端到端测试真实起文件监听 / HTTP 服务，断言延迟上限和重放无缺口。
- 跨平台差异（`/proc` vs `lsof`、FSEvents 路径）要有在当前平台实际跑的测试。
- 网页演示（`examples/web/index.html`）：必须同步 `crates/uniflo-gateway/src/index.html`（cargo 包内的内嵌副本），改动后跑 `scripts/verify.sh --e2e`。e2e 依赖的 DOM 钩子不能改名：`[data-check][data-ok]`、`.row[data-key][data-status]`、`#timeline [data-id][data-kind]`、`#more`、`#conn[data-state]`、URL 参数 `api` `token` `transport` `select`；用量、检索、管理、洞察视图的钩子（`data-k` / `data-v` 数值、`#v-usage` 的 `data-query`、`#i-heat rect[data-day]` 等）以 `scripts/demo-e2e.mjs` 里的选择器为准。迭代时可用 `E2E_ONLY=runUsage,runVisual` 只跑部分步骤，交付前跑全量。
- 演示页保持单文件、免构建、无外部运行时依赖、可离线；组件库固定版本内嵌，并保留上游许可证与来源。对外文本一律先转义再拼 HTML，链接只允许 http(s)。
