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
| 本机真实数据性能 | 见下节：中文 p50 6.6 ms / p95 26.0 ms，代码 p50 26.0 ms / p95 84.1 ms，合计 p50 17.9 ms / p95 69.1 ms；索引与源数据之比 0.32；全量构建 819,679 ms；输出只有计数与耗时 | PASS |

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
- 延迟依赖 OS 页缓存：冷缓存时，同一个 bm25 扫描在 sqlite3 CLI 里第一次 1.2–1.7 s，第二次 0.11–0.14 s。上面的基准包含了选词时的预查，冷缓存场景没有单独计入 p50/p95。
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
