# 开发规范

> 模块边界、代码风格、性能预算和隐私红线；改任何 crate 前读。

状态：`current` · 更新：2026-10-09

## 模块边界

| crate | 职责 | 允许依赖 |
|---|---|---|
| `uniflo-schema` | wire 类型（`Session`/`Event`/`Envelope`），唯一对外契约 | serde、serde_json |
| `uniflo-core` | 适配器 trait、JSONL 驱动、状态机、引擎、缓存、文件监听、进程探测 | schema |
| `uniflo-search` | 查询语法解析 + 模糊排序；全文索引 `fts`（SQLite FTS5，读 `Engine`） | schema、core |
| `uniflo-adapters` | 每个 harness 一个模块，每个模块一个 cargo feature | core、schema |
| `uniflo-gateway` | HTTP/SSE/NDJSON/WS 网关、Host/Origin/token 守卫 | core、search、schema |
| `uniflo-cli` | `uniflo` 二进制：守护进程 + 查询客户端 | 以上全部 |

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
| 全文检索（`/v1/search`，≥ 3 字符的词，索引建成后） | p50 < 50 ms、p95 < 200 ms | 另起端口的守护进程建完索引后，按词频挑 10 个中文词 + 10 个代码子串各查 5 次，只记耗时与计数 |
| 只有短词（< 3 字符）的全文检索 | 常见词 < 1 s；最多 2 s，超时返回 `partial` | 同上，按 `/v1/search` 实测；预算常量 `LIKE_BUDGET` |
| 新事件可被全文检索到 | < 2 s | `crates/uniflo-search/tests/fts.rs` |

引擎循环里不得做阻塞 IO：读文件、`ps`/`lsof`、SQLite 都走 `spawn_blocking`，结果回到单写者循环再写状态。

## 隐私与数据安全

- harness 数据只读；任何代码、测试、脚本不写、不移、不锁会话文件和数据库。
- 测试夹具一律手写合成数据（见各适配器 `tests` 模块与 `common::testkit`）；不提交真实会话片段，哪怕脱敏。
- 调试真实数据只输出计数、键名、类型分布；日志里不打印会话正文。
- 凭据只走环境变量（`UNIFLO_TOKEN`）或钥匙串；不进源码、日志、产物。

## 测试

- 适配器：每个模块至少覆盖一个完整回合（用户 → 工具调用 → 工具结果 → 回复 → 结束）的事件序列、状态、元数据，以及 `unknown` 为空。
- 引擎与网关：端到端测试真实起文件监听 / HTTP 服务，断言延迟上限和重放无缺口。
- 跨平台差异（`/proc` vs `lsof`、FSEvents 路径）要有在当前平台实际跑的测试。
- 网页演示（`examples/web/index.html`）：必须同步 `crates/uniflo-gateway/src/index.html`（cargo 包内的内嵌副本），改动后跑 `scripts/verify.sh --e2e`。e2e 依赖的 DOM 钩子不能改名：`[data-check][data-ok]`、`.row[data-key][data-status]`、`#timeline [data-id][data-kind]`、`#more`、`#conn[data-state]`、URL 参数 `api` `token` `transport` `select`。
- 演示页保持单文件、免构建、无外部运行时依赖、可离线；组件库固定版本内嵌，并保留上游许可证与来源。对外文本一律先转义再拼 HTML，链接只允许 http(s)。
