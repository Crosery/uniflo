# 2026-10-09 worklog · fulltext-search

> service-expansion 子任务 fulltext-search：SQLite FTS5 trigram 全文索引、`/v1/search`、`events?around=`、`uniflo grep`。测试只用手写合成数据；真实数据只记计数与耗时。

状态：`historical` · 更新：2026-10-09

## 完成条件

规格：`docs/comet/changes/service-expansion/specs/fulltext-search/spec.md`（Supervisor 工作区，只读）。

- 7 个 Scenario 各有自动化测试或真实数据记录，见下表。
- 本机真实数据：后台索引建完后，10 个中文词 + 10 个代码子串各查 5 次，p50 < 50 ms、p95 < 200 ms；记录索引大小与源数据之比、全量构建耗时；输出只含计数与耗时。
- `scripts/verify.sh --e2e` 通过。

## 设计要点

完整取舍见 `docs/decisions/ADR-0008-全文检索索引.md`，下面只记过程中的实测依据。

- 依赖：复用工作区已有的 `rusqlite`（bundled，SQLite 3.50），没有新增生产依赖。`uniflo-search` 新增依赖 `uniflo-core`（读 `Engine`），已写入 `docs/architecture.md` 和 `DEVELOPMENT.md`。测试 `fts::store::tests::bundled_sqlite_has_fts5_trigram` 确认 bundled 库带 FTS5 和 trigram 分词器。
- `detail=none` / `detail=column` 的实测结果：trigram 下短语 MATCH、`bm25()`、`snippet()` 都报 `phrase queries are not supported (detail!=full)`，所以只能用 `detail=full`。本机 Homebrew 的 sqlite3 不带 fts5，这项实验用 `/usr/bin/sqlite3` 和 bundled 库做。
- 写入性能（`sample` 符号化采样，真实数据构建期间）：
  - 带同步触发器时，3407 个 upsert 采样里有 2731 个落在 `fts5SavepointMethod`。原因是触发器开语句日志，FTS5 每遇到一个 savepoint 就刷一次段。改为显式维护 `docs_fts` 后，store 插入从 4004 提到 11834 docs/s。
  - 每页一个事务时，提交和 BEGIN 会和引擎里 OpenCode / Hermes 适配器的只读 SQLite 争 `unixShmBarrier` 进程级互斥。改成每 500 ms 提交一次的滚动事务后，端到端从约 1500 提到约 2100 events/s。
- 查询性能：最初 bm25 排序要 JOIN `docs` 拿会话和 kind，命中 1.4 万行时约 0.7 s。改为行号编码 `sid << 32 | seq << 3 | kind`、只扫 FTS 索引后，同类查询降到 10–60 ms。近因加权改为每个会话按 `updated_at` 计算一次。
- 一个测试偶发失败（`indexing:false` 但 `total:0`）：查询先扫描、后读 `indexing`，中间写线程刚好提交完并清了标志。改为先取 `(indexing, progress)` 快照再扫描，写线程保证先提交再清标志。
- `core::cache::dir()`：缓存目录在设了 `UNIFLO_HOME` 时随之移动。`index-v1.json` 也改用它，以前测试设了 `UNIFLO_HOME` 仍会写真实缓存。
- `scripts/daemon-smoke.mjs` 用真实家目录启动守护进程，所以加 `--no-fts`（不碰真实缓存），并断言 `/v1/search` 返回 503。

## Scenario → 证据

| Scenario | 证据 | 结果 |
|---|---|---|
| 中文与代码子串命中 | `crates/uniflo-search/tests/fts.rs::chinese_code_and_truncated_tool_output`：查 "缓存击穿"、"parse_rel"、"E0382" 和第 3 KB 处的关键词，各命中对应会话与事件，片段含 `\u0002…\u0003`；第 8 KB 处的词查不到 | PASS |
| 过滤、排除、短语与排序 | `fts.rs::filter_exclusion_phrase_and_recency`：今天的会话排在 10 天前的前面；`filter=h:` 只剩一个；短语只命中相邻出现；`-rollback` 排除；外加 10 个填充会话，避免语料太小时 bm25 的 IDF 被钳到接近 0 | PASS |
| 增量更新与 partial 覆盖 | `fts.rs::live_appends_and_partial_overwrites`：追加含 "zephyrquartz" 的行后 2 s 内命中（断言 < 2 s）；Gemini 同 id 由 "alpha" 改写为 "alphabet" 后 "alphabet" 只命中 1 条，旧内容没有独立命中；删除会话文件后检索为空。`store.rs::upsert_replaces_by_id_and_drops_emptied_rows` 覆盖行级覆盖 | PASS |
| 定位到事件窗口 | `crates/uniflo-gateway/tests/search.rs::search_hit_opens_a_centred_window`：`around=<命中 id>&limit=20` 返回 20 条，锚点下标 9；未知 id 返回 404；`crates/uniflo-core/src/window.rs` 4 个单元测试覆盖两端不足时的补齐 | PASS |
| 后台构建、格式升级与关闭 | `fts.rs::background_build_format_bump_and_incremental_restart`（2000 事件，`indexing:true` 带进度；重启只补 1 个变化的会话；格式号加一后 `rebuilt`）；`gateway/tests/search.rs::health_is_immediate_while_the_index_builds`（40 会话 × 50 事件，`/v1/health` 立即 200，`/v1/search` 返回 `indexing:true`）；`search_without_index_is_503`；`crates/uniflo-cli/tests/grep.rs::no_fts_answers_503_and_creates_no_index`（真实二进制 `--no-fts`：503 + 原因、`/v1/stats.fts` 为 null、不创建索引文件） | PASS |
| CLI grep | `cli/tests/grep.rs::grep_matches_rest_and_renders_sessions`（真实二进制：`grep 缓存击穿 --json` 与 `GET /v1/search?q=缓存击穿` 的会话和事件 id 一致；可读输出每个会话一行标题、下面是片段；非终端无 ANSI；`--filter`；无守护进程时进程内路径结果相同）；`render.rs::snippet_highlight_follows_the_terminal`（终端里转成 ANSI 高亮） | PASS |
| 本机真实数据性能 | 首版见下节（选词时的预查不计入，独立验收按字面协议判为不达标）。按字面协议（首次冷查询计入、词集从未查过）的结果见文末「独立验收失败后的修复」：最终版本 4 组合计 p50 11.7–18.4 ms、p95 61.6–89.3 ms，代码 p50 28.5–36.8 ms；索引与源数据之比 0.32；全量构建 819,679 ms + 合并约 3 min；输出只有计数与耗时 | PASS |

## 本机真实数据

- 测试守护进程：release 二进制在 `127.0.0.1:7421` 上运行，用真实家目录，参数 `--no-cache --no-update-check`。7311 上 launchd 管理的用户守护进程全程未动。测完已停掉，7421 已释放。
- 所有命令只输出计数和耗时，不打印片段或正文。

### 数据量与构建

- 源数据：6557 个会话，5809 个源文件或数据库，合计 17,653 MB。算法：取 `/v1/sessions` 每条的 `source` 去重后逐个 `stat` 求和，含 SQLite 的 `-wal`。
- 全量构建（release，节流系数 0.5）：1,339,836 个事件，`/v1/stats.fts.build_ms` = 819,679（13.7 min）。构建期间机器负载 15–22（14 核，其他 builder 同时在编译），`/v1/health` 1.5 ms。
- 索引大小：
  - 建完时 `bytes` = 5,631,298,592（含 WAL）。
  - 停机后 db 5,567,983,616 + wal 8,825,072 + shm 32,768，约 5.58 GB。
  - 与源数据之比 0.32。
  - 文件在 `~/Library/Caches/uniflo/fts-v1.sqlite`，留在真实缓存里。它是 Uniflo 的派生数据，可随时删除，删后下次启动重建。
  - 同一份数据在行号编码之前的旧方案下是 5,172.5 MB、748,006 ms。新方案的行号间隔更大，doclist 的 varint 变长，换来了查询时不用 JOIN。
- 增量重启：同版本重启 3 次，每次后台只剩 1–5 个会话待补，数秒追平，`rebuilt:false`。
- 建完后守护进程 RSS 136 MB。

### 查询延迟

脚本 `python3 -I /tmp/uniflo-fts-bench/scripts/bench.py`，放在仓库外。
- 选词：中文、代码候选各 20 个，由我拟定；先查一遍各自命中的会话数，去掉 0 命中的，再按命中数高低间隔取 10 个。含空格的代码串作为一个子串，按短语（`"…"`）发送。
- 每个词查 5 次，算上选词时的一次预查。p95 取排序后第 ⌈0.95n⌉ 个。

最终版本（机器负载约 19）：

| 中文词 | 命中会话 | 5 次（ms） |
|---|---|---|
| 环境变量 | 2055 | 26.3 25.0 29.4 26.0 10.1 |
| 版本号 | 1039 | 11.0 5.1 5.0 13.7 6.6 |
| 文件路径 | 922 | 20.2 23.9 7.6 22.5 23.4 |
| 单元测试 | 733 | 9.7 3.5 3.5 19.2 4.1 |
| 部署脚本 | 403 | 3.6 17.5 3.5 18.8 23.5 |
| 测试用例 | 322 | 3.0 2.4 1.9 2.6 19.1 |
| 用户体验 | 77 | 1.7 22.4 1.0 1.0 5.7 |
| 权限校验 | 64 | 1.2 0.8 0.9 3.2 2.9 |
| 工作流程 | 46 | 2.8 4.2 10.9 6.5 4.8 |
| 日志输出 | 32 | 8.2 11.6 8.5 11.9 6.6 |

| 代码子串 | 命中会话 | 5 次（ms） |
|---|---|---|
| `README.md` | 3578 | 112.6 78.4 84.1 80.0 69.1 |
| `"npm run"` | 1208 | 44.8 28.7 26.9 29.1 35.8 |
| `localhost:` | 862 | 31.1 33.7 25.9 30.3 26.6 |
| `useState(` | 286 | 57.0 50.6 57.4 94.7 67.2 |
| `TODO:` | 134 | 6.3 3.0 2.8 18.3 3.9 |
| `#[derive` | 88 | 6.9 21.1 5.6 17.4 9.9 |
| `"use std::"` | 77 | 30.5 24.7 18.4 27.4 24.6 |
| `"cargo test"` | 69 | 41.8 20.9 26.2 37.8 30.1 |
| `println!` | 52 | 11.1 4.3 24.5 22.6 5.3 |
| `"fn main"` | 16 | 13.4 18.4 20.7 12.3 25.7 |

| 版本（同一份索引、同一脚本） | 中文 p50 / p95 | 代码 p50 / p95 | 合计 p50 / p95 |
|---|---|---|---|
| 首版：每条命中单独跑 FTS5 `snippet()`，每个语句各开一个读事务 | 30.0 / 136.2 | 139.4 / 964.4 | 55.1 / 441.0 |
| 一次查询只开一个读事务 | 8.4 / 35.1 | 60.7 / 162.4 | 21.5 / 122.1 |
| 再加上片段从存储正文截取（最终版本） | 6.6 / 26.0 | 26.0 / 84.1 | 17.9 / 69.1 |

首版的 `sample` 采样（带符号的 release 构建，放在仓库外的 target 目录）：
- 片段阶段 486 个采样里有 355 个停在 `walIndexReadHdr → unixShmBarrier → __psynch_mutexwait`。这是 SQLite 进程级 VFS 互斥锁，引擎的 OpenCode 适配器在 `unixShmMap → open` 期间持有它。
- 扫描阶段基本都在 `fts5Bm25Function → sqlite3Fts5StorageDocsize → pread`。
- 第二版的采样中，每条命中各跑一次 `fts5MultiIterNew / fts5SegIterSeekInit`（每个 trigram × 18 个段），这是剩下的主要开销；第三版把它去掉了。

### 短词与其他接口

- 短词（< 3 字符，走 LIKE 全表扫描），已知限制：
  - 最终版本：`缓存` 20.5 s / 26.0 s（2710 个会话），`fn` 39.0 s / 15.4 s（2931 个会话），机器负载 39–48。
  - 首版在负载约 20 时：`缓存` 22.4 / 14.2 / 14.1 s，`性能` 约 10.3 s，`测试` 约 12 s。
  - 用 `/usr/bin/sqlite3 -readonly` 对照：`count(*) … LIKE '%缓存%'` 墙钟 34.3 s，而 user 2.0 s + sys 1.4 s，瓶颈是读约 1.7 GB 的 `docs` 表（IO）。
  - 规格要求短词走 LIKE，本次不另建 bigram 索引；已写进 `docs/search.md` 与 ADR-0008。
- `uniflo grep`（真实二进制，`--url http://127.0.0.1:7421`）：
  - `grep 环境变量 --json`：total 2055，20 个会话，60 条命中，`indexing:false`，耗时 0.17 s。
  - `grep 环境变量 -n 5`：5 行会话 + 15 行命中，stderr 为 `5 of 2055 matching sessions`。
- `events?around=`：对 `环境变量` 的前 5 条命中各取 `limit=20` 的窗口，都是 20 条，锚点下标都是 9，耗时 8–183 ms（要从 harness 文件读历史）。

## 验收命令

`scripts/verify.sh --e2e`：通过，exit 0，墙钟 42.8 s（user 105 s，机器负载约 40）。

- rustfmt、clippy `-D warnings`、16 个适配器 feature 单独编译、分支不变量：全部通过。
- workspace 测试：150 passed，0 failed。新增的测试：
  - `uniflo-search`：新增单元 7 个（另有原有的 5 个）、集成 5 个。
  - `uniflo-gateway/tests/search.rs`：3 个。
  - `uniflo-cli/tests/grep.rs`：2 个。
  - `uniflo-core::window`：4 个。
  - `uniflo-schema::search`：1 个。
  - `uniflo-cli::render`：1 个。
- release 构建与 daemon smoke 通过，含 `/v1/search answers 503 with --no-fts`。
- demo e2e：34 passed，0 failed（SSE / WS / NDJSON 实时追加 6 / 9 / 5 ms）。

## 残余风险与未验证

- 标签含 Uniflo 版本（规格要求，与 `index-v1.json` 一致）：每次升级都会后台重建一次，约 14 min、约 2/3 个核，并重写 5.6 GB。
- 同一缓存目录有两个守护进程时，会争用同一个索引文件；版本不同时还会互相判定标签不匹配而重建。AGENTS.md 已写明。
- 只有短词的查询：已改为有界扫描，见下文「短词扫描有界化」。罕见短词在 2 s 内可能找不全（带 `partial`）。扫描能提前停，依赖「会话 `updated_at` ≥ 其事件 ts」；若某 harness 的事件时间晚于会话的 `updated_at`，这一页的排序可能有偏差（未见实例，未验证）。
- 延迟依赖 OS 页缓存：冷缓存时，同一个 bm25 扫描在 sqlite3 CLI 里第一次 1.2–1.7 s，第二次 0.11–0.14 s。上面的基准包含了选词时的预查，冷缓存场景没有单独计入 p50/p95（已由文末「独立验收失败后的修复」处理）。
- 没有守护进程时，`uniflo grep` 在本进程里补齐索引；落后很多或首次运行时要几分钟（首建约 14 min，不节流时会更快，未单独测）。
- 未验证：
  - Linux / Windows 的缓存目录与索引行为（只在 macOS 上跑过）。
  - 多个客户端同时检索时的延迟（基准是串行请求）。
  - `uniflo grep` 不连守护进程、在真实数据上的首次构建耗时。

## 短词扫描有界化（team-lead 跟进，19:10–19:30）

问题：只有 < 3 字符的词时要全表 LIKE，本机 15–40 s。中文 2 字词（缓存、部署、报错）恰恰最常见。

完成条件：
- 常见短词凑够一页就停，远低于 1 s；
- 扫描有 2 s 预算，超时返回已找到的部分，带 `partial: true` 和 `scanned_until`；
- schema、文档、CLI 提示同步；
- 合成数据测试覆盖提前停止和预算耗尽；
- 真实数据重测 5 个常见 + 5 个罕见的 2 字词。

改动：
- `Fts::scan_like`：按会话 `updated_at` 从新到旧，在每个会话的行号区间上执行 `LIKE`，每 1024 行一条语句。用一个最小堆维护前 `offset + limit` 个会话的最新命中时间；下一个会话的 `updated_at` 不晚于堆顶时停止，此时这一页是精确的，`total` 为下限，带 `scanned_until`。每条语句之间检查 `LIKE_BUDGET`（2 s，`FtsOptions::like_budget` 可注入），超时返回 `partial: true`。
- 带 ≥ 3 字符词的查询路径不变。
- `SearchResponse` 新增可选字段 `partial`（false 时不输出）和 `scanned_until`，只增不改。
- `uniflo grep` 的统计行在提前停止时显示 `N of M+`，`partial` 时多打一行提示。
- 同步 docs/schema.md、api.md、search.md、ADR-0008、DEVELOPMENT.md 性能预算。

测试：
- `crates/uniflo-search/tests/fts.rs::short_terms_stop_early_and_within_their_budget`：
  - 30 个会话各含「缓存」，`limit=5` 得到最新的 5 个会话；会话内命中按新到旧；`total < 30` 且带 `scanned_until`；翻第二页得到接下来的 5 个。
  - 罕见词「鳕鱼」完整扫描，无 `scanned_until`。
  - `like_budget = 0` 时同一查询返回 `partial: true`、结果为空。
  - ≥ 3 字符的词不受预算影响（total 30）。
- `grep::tests::footer_flags_lower_bounds_and_partial_scans`，以及 schema 线上形状测试的新字段。

真实数据（release，`127.0.0.1:7421`，真实家目录，测完已停；只输出计数与耗时）：
- 基准：先用 `/usr/bin/sqlite3 -readonly` 在索引上一次全表扫描，数出各词实际命中的会话数。耗时 54.1 s（user 29.8 s）。
- 每个词查 5 次（`/tmp/uniflo-fts-bench/scripts/short.py`），机器负载 12–20。

| 常见词 | 实际会话数 | 5 次（ms） | 结果 / total | partial | 往回扫到 |
|---|---|---|---|---|---|
| 缓存 | 2710 | 993.9 27.9 27.9 27.5 29.0 | 20 / 22+ | false | 0.2 天 |
| 部署 | 2882 | 85.7 31.9 32.7 33.6 30.4 | 20 / 25+ | false | 0.3 天 |
| 报错 | 2099 | 29.1 27.9 28.9 28.4 28.6 | 20 / 21+ | false | 0.3 天 |
| 测试 | 3879 | 24.5 22.6 24.0 21.2 24.0 | 20 / 21+ | false | 0.2 天 |
| 性能 | 1400 | 344.3 78.7 63.3 65.1 64.4 | 20 / 24+ | false | 0.6 天 |

常见词合计 n=25：p50 29.0 ms，p95 344.3 ms，最大 993.9 ms（缓存冷时的首次）。

| 罕见词 | 实际会话数 | 5 次（ms） | 找到 | partial | 往回扫到 |
|---|---|---|---|---|---|
| 火锅 | 3 | 2010.3 2002.5 2002.5 2003.1 2000.9 | 2 | true | 13.1 天 |
| 足球 | 4 | 2008.2 2001.6 2029.8 2002.8 2005.3 | 3 | true | 13.4 天 |
| 诗歌 | 9 | 2002.8 2008.4 2002.0 2000.9 2005.5 | 3 | true | 25.1 天 |
| 瑜伽 | 12 | 2010.6 2006.2 2001.2 2003.3 2001.4 | 3 | true | 27.7 天 |
| 熊猫 | 15 | 2002.3 2006.6 2040.5 2002.4 2010.7 | 7 | true | 25.1 天 |

罕见词合计 n=25：p50 2003.1 ms，p95 2029.8 ms，最大 2040.5 ms。

- 罕见词全部按预算在约 2 s 返回 `partial`，2 s 内往回覆盖约 13–28 天的会话；更早的命中要靠加长检索词。
- `uniflo grep`（真实二进制）：
  - `grep 缓存 -n 5`：5 个会话、14 条命中，stderr 为 `5 of 10+ matching sessions`。
  - `grep 火锅 -n 5`：1 个会话，stderr 为 `1 of 1+ matching sessions`，并另起一行提示时间预算耗尽、某时间点之前的会话未搜索、可加长检索词。
- 回归：同一 daemon 重跑 `bench.py`。
  - ≥ 3 字符的词：中文 p50 3.8 / p95 23.2 ms，代码 19.8 / 71.6 ms，合计 12.7 / 64.6 ms，没有退化。
  - 其中的短词：缓存 200.7 / 44.2 / 38.0 ms，性能 131.3 / 71.8 / 93.0，测试 25–28，fn 25–35，ok 23–26。
- `scripts/verify.sh --e2e`：
  - 第一次失败：clippy `items-after-test-module`，因为 `grep.rs` 的测试模块不在文件末尾；移到末尾后修复。
  - 重跑通过，exit 0，墙钟 28.9 s；workspace 测试 152 passed，0 failed；e2e 34 passed，0 failed；smoke 中 `--no-fts` 下的 503 断言通过。

## 提交

每个提交单独执行过 `cargo check --workspace --all-targets`：其余改动先 stash 掉，检查通过后再恢复。

- `3af9732` feat(schema): 新增全文检索响应类型
- `d4d4870` fix(core): 缓存目录随 UNIFLO_HOME 移动
- `db29d04` feat(core): 按事件 id 取居中的事件窗口
- `555f8a2` feat(search): SQLite FTS5 trigram 全文索引与查询
- `ef25fca` feat(gateway): 新增 /v1/search 与 events?around= 窗口
- `8b1ea12` feat(cli): 新增 uniflo grep 与 daemon --no-fts
- `c91cbf9` feat(search): 只有短词的全文检索改为有界扫描

没有推送、建分支或打 tag。7421 上的测试守护进程已停止；7311 上的用户守护进程未动。真实缓存里留有 `fts-v1.sqlite`（约 5.58 GB），可以删除。

## 独立验收失败后的修复（runId f7706ff3，约 20:00–21:10）

独立验收（Verifier）的结论：

1. Scenario 7 按字面协议不达标。协议是每个词查 5 次、第一次（冷）计入，词集在该守护进程里从未查过。6 组里 5 组 p95 129–451 ms，代码 p50 24–53 ms、p95 199–626 ms，首次查询 50–800 ms。
2. D1（major）：长词加短词的混合查询没有预算、不会提前停。本机 `fn main`、`async fn`、`if err`、`fn new` 要 8.5–18.7 s。
3. D2（minor）：同分会话的顺序随 `HashMap` 遍历变化，翻页会重复或漏掉，CLI 与 REST 的同一页也可能不同。

完成条件：
- 至少 3 组新词集按字面协议：合计 p50 < 50 ms 且 p95 < 200 ms，代码子串 p50 < 50 ms。
- 上面 4 个混合查询 1 s 内返回完整结果，或在预算内返回 `partial`。
- 同分顺序固定，翻页不重不漏。
- 合成测试覆盖混合查询的提前停止与预算、同分翻页稳定。
- `scripts/verify.sh --e2e` 通过。

### 改动

- D1：`scan_like` 改为 `scan_recent`。
  - 有短词的查询（含 `-` 排除的短词）都走这条路径，按时间倒序。
  - 同时有长词时，先从 FTS 只取行号（不算 bm25、不读正文），按会话分组；再按会话 `updated_at` 从新到旧，对这些行逐行 `LIKE` / `NOT LIKE`。
  - 提前停止和 2 s 预算与只有短词时相同。
- D2：会话按分数、`updated_at` 倒序、会话 key 排成全序。会话内命中按分数、再行号。提前停止的条件由"下一个会话不晚于第 N 个最新命中"改为"严格早于"，因为时间相同的会话可能在 key 比较上胜出。
- 性能：
  - 单个短语用 FTS5 C API 注册的 `uniflo_bm25()` / `uniflo_rows()`，省掉内置 `bm25()` 在全表上统计 IDF 的那一遍（`xQueryPhrase`）。IDF 在取完行后乘上，分数与内置相同（`rank::tests`）。
  - 积压清空后，若最大段之外的叶子页超过四分之一（`store::scattered`），写线程在空闲轮次里分步 `('merge', -2000)` / `('merge', 2000)`，每步单独提交，并按耗时 × 0.5 节流。
  - 合并结束后预读 `docs_fts_idx` 与 `docs_fts_docsize`（约 24 MB）。
  - 读连接页缓存 32 MB。
  - `FtsStatus.warming`：合并加预读期间为 true（schema 只增）。

### 诊断依据（只记计数与耗时）

- 机器：48 GB 内存，空闲页约 70 MB，文件页约 8.7 GB，负载 10–26（其他 builder 同时在编译）。
- 索引布局（`dbstat`）：`docs_fts_data` 3615 MB、`docs` 1608 MB、`docs_fts_docsize` 18 MB、`docs_fts_idx` 6 MB。建完后分在 18 个段里，其中 7 个大段。
- 冷首查的时间几乎都在扫描阶段，命中片段阶段是 5–60 ms。
  - 同一个新词在 sqlite3 CLI 新进程里，第一次 77–229 ms，第二次 17–19 ms，sys 时间很小，说明是在等 IO。
  - 守护进程内临时计时显示，常见代码词首查的扫描阶段 400–570 ms。
- 内置 `bm25()` 的 IDF 统计约占查询时间的 38%（`sample`）。
- 在索引副本上一次 `optimize`：冷查询快 2–6 倍，热查询不变；但耗时 2 min 14 s，WAL 峰值约 3.7 GB。
- `mmap_size` 2 GB：同一热查询各 6 次，开 50–82 ms、不开 59–76 ms，没有区别，未采用。

### 协议（照字面）

- 守护进程：release 二进制，`uniflo daemon --bind 127.0.0.1:7421 --no-cache --no-update-check`，真实家目录，索引就是真实缓存里的 `fts-v1.sqlite`。7311 上的用户守护进程全程未动。
- 开始测的时机：`/v1/stats` 的 `fts.indexing` 和 `fts.warming` 都为 false。第 17 组例外，在 `indexing:false` 后立刻测，那时预读还没完成。
- 选词：
  - 候选是我拟定的通用词：中文 ≥ 3 字、代码子串 ≥ 3 字符，共 5 批。
  - 凡在此前任何一次测量（包括旧脚本）中查过的词，以及与它们互为子串的词，都排除。
  - 词频 = 1/20 抽样会话（`sid % 20 = 3`）中含该词的会话数，用 `/usr/bin/sqlite3 -readonly` 在 `docs` 上按行号区间 `LIKE` 统计。这一步只读抽样会话的 `docs` 页，不读 FTS 索引。
  - 去掉 0 命中的词，按词频从高到低取 10 × k 个，轮流分进 k 组，所以每组都覆盖从高频到低频。
- 跑法：`python3 -I /tmp/uniflo-fts-bench/proto/run.py http://127.0.0.1:7421 <组号>`。
  - 每个词连续发 5 次 `GET /v1/search?q=`，含空格的词按短语 `"…"` 发送，5 次全部计入（第一次是冷查询）。
  - 客户端墙钟计时，只输出词、`total` 和耗时。
  - 每组 100 个样本，其中代码 50 个。p50 取中位数，p95 取排序后第 round(0.95n) 个。
  - 同一进程里各组的词互不重复，也没有任何按词预热。

### 结果（ms）

| 组 | 配置 | 中文 p50 / p95 | 代码 p50 / p95 | 合计 p50 / p95 | 首次 p50 / p95 / max | 达标 |
|---|---|---|---|---|---|---|
| 1 | 上一候选（ef9148a） | 3.1 / 51.4 | 43.8 / 395.2 | 10.9 / 168.9 | 64.3 / 515.3 / 604.5 | 是（p95 余量小） |
| 2 | + 读连接 mmap 2 GB、cache 32 MB | 9.6 / 61.9 | 36.5 / 300.3 | 22.0 / 120.9 | 67.8 / 422.7 / 518.7 | 是 |
| 3 | 同 2，测前用 sqlite3 CLI 全读三张 FTS 表（47 s） | 6.2 / 25.7 | 37.3 / 86.1 | 20.8 / 76.8 | 25.7 / 74.3 / 78.5 | 是 |
| 4 | 单短语 bm25 + 进程内预读整个索引（小表先读），18 段 | 5.3 / 38.6 | 37.3 / 467.2 | 20.2 / 193.4 | 51.0 / 550.5 / 636.0 | 是（p95 余量小） |
| 5 | + 段合并（179 s），预读同 4；预读刚结束后的第一组 | 2.6 / 181.1 | 28.3 / 728.7 | 7.3 / 391.2 | 136.7 / 1194.3 / 1614.9 | **否** |
| 6 | 同 5，随后 | 3.7 / 41.0 | 32.9 / 117.2 | 20.6 / 92.9 | 42.5 / 186.7 / 219.6 | 是 |
| 7 | 同 5，随后 | 4.2 / 25.9 | 30.4 / 163.8 | 21.9 / 149.8 | 69.5 / 192.5 / 218.7 | 是 |
| 8 | 同 5，随后 | 3.9 / 25.5 | 25.5 / 133.5 | 15.7 / 101.7 | 70.5 / 164.9 / 178.0 | 是 |
| 9 | 预读改为大表先、小表后；重启后第一组 | 2.3 / 25.6 | 26.8 / 115.8 | 18.8 / 57.3 | 29.9 / 136.0 / 287.6 | 是 |
| 10 | 同 9 | 4.1 / 22.3 | 27.8 / 222.2 | 15.5 / 134.0 | 45.4 / 222.2 / 329.2 | 是 |
| 11 | 同 9 | 2.5 / 21.9 | 27.1 / 137.7 | 18.5 / 88.9 | 33.0 / 148.5 / 155.4 | 是 |
| 12 | 同 9，去掉临时计时；重启后第一组 | 1.0 / 21.3 | 29.8 / 165.9 | 14.7 / 79.7 | 36.6 / 184.7 / 219.4 | 是 |
| 13 | 同 12 | 6.6 / 23.9 | 22.0 / 66.2 | 12.8 / 64.0 | 27.3 / 234.9 / 295.3 | 是 |
| 14 | 同 12 | 2.3 / 21.7 | 21.6 / 53.9 | 8.1 / 36.0 | 16.2 / 56.6 / 59.8 | 是 |
| 15 | 同 12 | 3.4 / 23.9 | 19.4 / 47.9 | 10.7 / 40.6 | 19.9 / 40.6 / 107.5 | 是 |
| 16 | 同 12 | 4.7 / 23.6 | 12.2 / 48.5 | 9.2 / 31.8 | 16.0 / 57.5 / 58.2 | 是 |
| 17 | 同 12；重启后 `indexing:false` 立即测，预读进行中 | 6.5 / 43.5 | 24.9 / 115.1 | 21.3 / 108.3 | 45.7 / 220.8 / 243.5 | 是 |
| 18 | 同 12，预读完成后 | 10.1 / 30.0 | 27.7 / 138.6 | 17.2 / 83.7 | 38.9 / 224.6 / 370.2 | 是 |
| 19 | 同 18 | 6.4 / 27.6 | 24.6 / 100.8 | 19.9 / 71.2 | 46.5 / 252.2 / 351.3 | 是 |
| 20 | **最终版本**：只预读 idx + docsize（2.8 s）；重启 4 s 后第一组 | 3.3 / 17.7 | 31.6 / 152.5 | 17.9 / 76.4 | 48.2 / 211.7 / 345.6 | 是 |
| 21 | 最终版本 | 2.9 / 22.8 | 30.0 / 112.9 | 18.4 / 63.6 | 31.9 / 123.6 / 177.3 | 是 |
| 22 | 最终版本 | 3.1 / 9.9 | 36.8 / 91.4 | 11.7 / 89.3 | 32.9 / 103.0 / 338.6 | 是 |
| 23 | 最终版本 | 2.0 / 8.8 | 28.5 / 116.3 | 12.8 / 61.6 | 24.6 / 117.8 / 171.1 | 是 |

- 负载：第 5–8 组约 9，第 9–11 组约 19，第 12–16 组约 22，第 17–19 组约 14，第 20–23 组 16–18。
- 第 1–4 组是排查用的中间版本，这几组的词也是第一次查。第 1 组用的正是上一候选，在本机这一组上勉强达标，而验收方的词集上 p95 129–451 ms，说明余量不够。
- 第 5 组不达标的原因：预读先读 24 MB 的小表、再读 3.6 GB 的大表，内存紧张时小表被挤出页缓存，第一组词的首次查询要回到磁盘上找文档长度。第 9 组起把小表放到最后读，问题消失。
- 第 17 组说明，合并成一个段之后，即使预读没做完也达标。
- 整个索引的预读要 30–40 s，还会把短词扫描要读的 `docs` 页挤出页缓存：重启后第一条短词查询 2009 ms（`缓存`），只预读小表时是 221 ms（`修改`）。所以最终版本只预读两张小表。
- 合并后：1 个大段加几个跟随写入产生的小段，空闲页 17,744–19,153 页（约 73–78 MB），文件 5.62 GB（含 WAL），与源数据之比仍为 0.32。合并期间 WAL 最大约 40 MB。
- 守护进程 RSS 127 MB。

### 混合查询（D1）

不加引号，每个查询 5 次、首次计入：

| 查询 | 最终版本（第一次查）5 次 | 稍后另一进程 5 次 | 结果 |
|---|---|---|---|
| `fn main` | 324.0 58.0 50.1 54.8 53.4 | 144.7 57.2 44.1 54.3 49.2 | 20 个会话，`total` 22+，`partial:false`，提前停 |
| `async fn` | 211.7 38.0 48.2 32.2 44.5 | 80.7 50.7 51.4 32.0 42.6 | 20 / 27+ |
| `if err` | 178.0 55.5 48.8 37.6 51.5 | 59.6 63.3 50.8 43.3 63.7 | 20 / 21+ |
| `fn new` | 155.6 36.5 48.8 64.0 35.0 | 88.7 57.3 46.2 48.8 38.7 | 20 / 22+ |

- 第一列在第 9–11 组的守护进程里测，是这 4 个查询在该版本上的第一次；第二列用最终版本测。
- `uniflo grep "fn main" -n 5`（真实二进制，只看 stderr）：`5 of 14+ matching sessions`。

### 顺序稳定（D2）

`浏览器`、`ls -la`、`fn main`、`ok` 各取 `limit=60`，与 3 页 `limit=20` 拼起来逐项相同，无重复；再查一遍 `limit=60`，结果相同。

### 短词回归

最终版本，`scripts/short.py`，负载约 16：
- 常见词 n=25：p50 41.5 ms，p95 87.0 ms，最大 347.7 ms，全部不 partial。
- 罕见词 n=25：约 2.0 s 返回 `partial`，与上一版本相同。
- 重启后没查过的常见短词，首次查询：`修改` 221.1、`运行` 49.0、`配置` 26.2、`数据` 31.0、`用户` 23.9 ms。

### 测试

- `crates/uniflo-search/tests/fts.rs::mixed_terms_stop_early_and_within_their_budget`：
  - `cache fn` 在 `limit=5` 时返回最新的 5 个会话，会话内命中按新到旧，带 `scanned_until` 且 `total < 30`；第二页是接下来的 5 个。
  - 只提到 cache、不含 fn 的 5 个更新的会话不出现。
  - 罕见短词 `cache zq` 扫完全部，结果完整。
  - `cache -fn` 走同一扫描，结果按时间倒序。
  - 预算为 0 时返回 `partial`、结果为空。
- `fts.rs::equal_scores_page_in_a_stable_order`：25 个同分会话，`tiebreak`（bm25）与 `平局`（短词扫描）都按 key 排序；`limit=7` 翻页 3 遍，拼起来与一次取全相同。
  - 反向验证 1：把提前停止改回"不晚于"，测试失败，第 1 页是 e18–e24。
  - 反向验证 2：去掉排序里 key 的比较，测试同样失败。
- `fts::store::tests::scattered_segments_merge_into_one`：三个段，判定为分散；分步合并到无事可做后，只剩一个段，查询结果不变。
- `fts::rank::tests::single_phrase_scores_equal_builtin_bm25`：自定义 bm25 乘以 IDF 后，与内置 `bm25()` 相等。
- `background_build_format_bump_and_incremental_restart` 新增断言：积压清空后 `warming` 会变成 false，且此时索引不再分散。

### 验收命令

`scripts/verify.sh --e2e`：通过，exit 0，墙钟 44.7 s，机器负载约 18。
- rustfmt、clippy `-D warnings`、各适配器 feature 单独编译、分支不变量：全部通过。
- workspace 测试 156 passed，0 failed。
- daemon smoke 通过，含 `--no-fts` 下的 503。
- demo e2e 34 passed，0 failed。

### 残余风险

- 首次冷查询仍受系统页缓存影响。本机内存紧张时，常见代码词的首次查询最多约 370 ms。只因为每个词只有第一次是冷的，p95 才能达标；页缓存更小的机器上余量会变小。
- 合并是一次 3 min 左右的后台 IO 和 CPU（本机 3.6 GB 索引），在全量构建或大批补建之后发生，期间 `warming:true`。
  - 判定是启发式的（最大段之外超过四分之一）。数据量涨到原来约 4/3 时会再触发一次全量重写，摊下来是常数倍的写放大。
  - 合并中途重启：下次启动会重新判定并接着合并，已经并入的部分可能要再读写一遍（未单独测）。
- 只有短词的查询，在重启后第一次读到冷的 `docs` 页时仍可能接近 2 s 预算（预算之内会带 `partial`）。
- 所有测量都在同一台 macOS 机器上，负载很高；Linux / Windows 未验证。
- 词集由我拟定，词频用 1/20 抽样统计。统计时用 `LIKE` 读过抽样会话的 `docs` 页，这只影响片段阶段，不影响 FTS 索引的冷热。

### 提交

- `d2bc987` fix(search): 含短词的混合查询改为有界扫描，同分结果全序。单独检查过：`cargo fmt --check`、clippy `-D warnings`，以及 uniflo-search、gateway、cli、schema 的测试。
- `780bde0` perf(search): 单短语 bm25 省去 IDF 统计，积压后合并段并预读小表。这一提交的树与跑 `verify.sh --e2e` 的树一致。

没有推送、建分支或打 tag。7421 上的测试守护进程已停止；7311 上的用户守护进程未动。真实缓存里的 `fts-v1.sqlite` 已合并成一个段，可以删除，删后下次启动重建。
