# 2026-10-09 worklog · session-cleanup

> service-expansion 子任务 session-cleanup：用户确认后先写精简归档、再把会话源文件移入系统回收站；归档会话照常列出、检索、计入用量；写接口安全边界与 `--read-only`；ADR-0006。真实数据只做只读计划与计数，从未执行清理。

状态：`historical` · 更新：2026-10-09

## 22:40 · 会话清理与写接口

### 完成条件

- `docs/comet/changes/service-expansion/specs/session-cleanup/spec.md` 的 8 个 Scenario 全部有自动化证据。
- 合成会话走"计划 → 确认 → 执行"：源文件进注入的回收站目录，归档 ≤ 源文件的 20%，归档会话仍出现在列表、全文检索、用量统计里，用量与清理前逐项相等。
- 测试与沙箱永不触碰真实回收站：测试注入 `DirTrash`，CLI 测试用 `UNIFLO_TRASH_DIR`；设了 `UNIFLO_HOME` 而没设 `UNIFLO_TRASH_DIR` 时拒绝移动。
- `scripts/verify.sh --e2e` 通过；`uniflo scan --no-cache --json` 的 `stats.unknown` 为空，`bad_lines`、`read_errors` 为 0。

### 改动与依据

- schema（只增）：`Session.archived`；`Event.truncated` 语义扩到"写入归档时被截短"；新模块 `crates/uniflo-schema/src/cleanup.rs`（`CleanupRequest` / `CleanupPlan` / `CleanupCandidate` / `CleanupTarget` / `CleanupReport` / `CleanupResult` / `ArchiveList` / `ArchiveEntry` / `ArchiveRemoved`）。
- core：
  - `archive/`：`ArchiveStore`（`index.json`、`cleanup.log.jsonl` 追加 + fsync、`tombstones.json`、原子写归档文件）、`compact`（16 KiB 正文 / 2 KiB 工具、base64 段替换、ruzstd 编解码）。
  - `cleanup/`：计划（10 min TTL，只读元数据）与执行（重新判定 → SHA-256 清单 → 归档 → 日志落盘 → 再比文件戳 → `Engine::retire` → 回收站 → 墓碑）；`Trash` trait 的 `SystemTrash` / `DirTrash` / `NoTrash`；`targets` 三个声明助手。
  - `engine_archive.rs`：归档会话进出会话表、`history()` 读归档、用量账本换成归档账本、墓碑接纳（`admit`）、`apply()` 丢弃墓碑下的读取；文件监听命中墓碑路径立即重扫。
  - `Adapter::cleanup_targets` / `LineDecoder::cleanup_targets`，默认不支持。
- 适配器：claude（含 qoder、qwen）、workbuddy、pi（含 omp、crosery、commandcode）、factory 用 `with_siblings`；reasonix 以去掉 `.events.jsonl` 的名字为 stem；codex、gemini、antigravity（只清转录）用 `file`；cursor、dsh 用 `parent_dir`；prime 加 `session-artifacts/<id>/`。支持矩阵测试 `crates/uniflo-adapters/tests/cleanup.rs`。
- search：`is:archived`（可取反）。
- gateway：`write.rs` 写守卫中间件（可复用，agent-access 的写接口挂同一子路由）；`cleanup.rs` 四个处理器；`router_with(engine, guard, Services)`；`/v1/health.read_only`；CORS 放行 `POST, DELETE` 与 `x-uniflo-write`；写请求 token 失败返回 403。
- CLI：`uniflo clean <key…|--query> [--dry-run] [-y] [--json]`、`uniflo archive [ls|rm <key>] [--json]`（`crates/uniflo-cli/src/clean.rs`），daemon `--read-only`；有守护进程时走 REST（带 `X-Uniflo-Write: 1`），否则进程内执行。
- 依赖：`trash` 5、`sha2` 0.10（已批准）；zstd 用已有的 `ruzstd`。
- 文档：ADR-0006（并修订 ADR-0004 的"网关只读"）、`AGENTS.md` 契约与环境事实、`docs/api.md#会话清理` 与 `#写接口`、`docs/schema.md#会话清理`、`docs/search.md`、`docs/adapters.md#会话清理`、`docs/architecture.md#会话清理与归档`、`docs/conventions/DEVELOPMENT.md`、`README.md`。

### 失败与教训

- 起初计划把单独请求的子代理"折叠"到父会话；与 Scenario 1（子代理应给出"需随父会话一起清理"）矛盾，改为逐个列出、原因 `subagent`。
- 还原测试第一次用了约 30 s：子代理目录的 FSEvents 事件落在墓碑路径下，被当成普通变化，只能等 30 s 兜底重扫。改为监听命中墓碑路径时立即发 `Rescan`，还原后 204 ms 即以源文件出现。
- 适配器矩阵测试里 reasonix 声明为 `None`：测试用的文件名没有 `.events.jsonl` 后缀。改测试路径为 `s/abc.events.jsonl`。
- clippy `result_large_err`：`Result<Arc<Cleanup>, Response>`，改为 `let Some(c) = … else { return disabled() }`。
- **误写了一次真实索引缓存**：调研时以真实 `HOME` 跑了 `cargo run -p uniflo -- harnesses --local --json`，`Source::open` 的本地模式把索引写回了 `~/Library/Caches/uniflo/index-v1.json`（同一标签、同一格式，常驻守护进程随后会自行覆盖，未见异常；harness 数据未受影响）。之后所有真实数据命令都隔离 `HOME`、`UNIFLO_DATA_DIR`、`UNIFLO_CACHE_DIR`，或只用不写缓存的 `scan --no-cache`。

### Scenario 证据

| Scenario | 证据 | 结果 |
|---|---|---|
| 计划阶段的可清理判定 | `crates/uniflo-gateway/tests/cleanup.rs::plan_judges_each_kind_of_session`：claude 父会话可清理且目标含子代理目录；opencode `unsupported`、codex `working`、pi `symlink`、子代理 `subagent`；`freed_bytes` / `archive_bytes` 有值；计划不改任何文件 | 通过 |
| 执行清理并保留精简归档 | 同文件 `execute_archives_then_trashes_and_the_archive_stays_usable`：两个目标进注入回收站，SHA-256 与日志清单一致；`CheckedTrash` 在每次移动时确认清单已在日志里；归档 2078 B / 源 1 966 134 B（0.11%）；`freed_bytes`、`archive_bytes` 与磁盘一致 | 通过 |
| 归档会话继续可用 | 同上：列表只出现一次、`archived: true`、idle；事件 id 与清理前相同，长输出与 base64 参数标 `truncated`；已有全文索引仍命中、新建索引能从归档读到子代理正文；`/v1/sessions/{key}/usage` 与 `/v1/usage?group_by=session` 与清理前相等；`is:archived` / `!is:archived` | 通过 |
| 计划后源文件变化则拒绝 | `changed_source_and_expired_plans_are_refused`：追加一行后执行 → `failed` / `source_changed`，源文件不动、无归档；过期计划 410 | 通过 |
| 从回收站还原后以源文件为准 | `restoring_from_the_trash_brings_the_source_back`（守护进程默认 30 s 重扫）：204 ms 后以源文件出现、无 `archived`、只列一次；归档文件仍在，`GET /v1/archive` 标 `restored` | 通过 |
| 写接口安全边界 | `write_endpoints_need_every_condition`：外站 Origin（即使 `--cors-origin '*'`）、缺 `X-Uniflo-Write`、非回环 Host、有 token 未带、`--read-only` 均 403；合法请求 200；`read_only` 为 true / false；`write.rs` 单测；CLI 测试对真实二进制的 `--read-only` 守护进程 | 通过 |
| CLI clean 与 archive | `crates/uniflo-cli/tests/clean.rs`：`--dry-run` 不改文件；非交互不带 `--yes` 退出码非 0；`--yes` 结果与 `/v1/archive` 一致；`archive ls` 列出大小；`archive rm` 后归档文件删除、会话从列表消失；无守护进程的进程内路径；无 `UNIFLO_TRASH_DIR` 的沙箱 → `trash_failed`、文件不动、归档撤销 | 通过 |
| 文档与契约 | ADR-0006、ADR-0004 修订、`AGENTS.md`、`docs/api.md`、`docs/schema.md`、`docs/search.md`、`docs/adapters.md` | 通过（人工核对） |

### 验证命令与结果

- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e`：rustfmt、clippy `-D warnings`、每个适配器 feature 单独编译、分支不变量、全量测试（211 个测试全部通过）、release 构建 + daemon smoke、网页演示 e2e `{"ok":true,"passed":34,"failed":0}`；最后一行 `verify: all checks passed`。
- `cargo test -p uniflo-gateway --test cleanup -- --nocapture`：5 passed（2.31 s），`archive 2078 B of source 1966134 B`、`restored source listed after 204 ms`。
- 真实数据 `UNIFLO_DATA_DIR=<tmp> ./target/release/uniflo scan --no-cache --json`：`sessions` 6573、`sources` 5833、`bad_lines` 0、`read_errors` 0、`unknown` `{}`、`index_ms` 2157；数据目录未被创建。
- 真实数据只读计划：release 守护进程 `--bind 127.0.0.1:7461 --no-fts --no-update-check --no-price-sync`，`HOME`、`UNIFLO_DATA_DIR`、`UNIFLO_CACHE_DIR`、`UNIFLO_CONFIG_DIR` 指向临时目录，`UNIFLO_HOME=<真实家目录>`，未设 `UNIFLO_TRASH_DIR`（即使误执行也是 `NoTrash`）。只调用 `POST /v1/cleanup/plan`，从未执行；结束后删除临时目录。
  - `q=is:root`：3064 个会话，可清理 2092，可释放 10 945 825 565 B，预计归档 1 094 583 491 B，耗时 4.32 s（后台用量索引同时在建）。不可清理：claude `live_process` 9、`working` 3；codex `hardlink` 226（都在 `sessions/` 下）；omp `live_process` 5；workbuddy `live_process` 1；SQLite 型 hermes 23、kilo 3、mimocode 594、minimax 13、opencode 91、zcode 4 均 `unsupported`。
  - `q=is:root before:30d`：1910 个，可清理 1100，可释放 1 688 085 952 B，耗时 0.93 s。
- 真实数据精简比例（只读探针，仓库外的临时 crate：引擎 `history()` 读全量事件 → `compact` → `encode`，只在内存里算大小，不写任何文件；每个 harness 取最大的 15 个根会话，归档只算根会话，子代理未计入，所以 claude / prime 是下限）：合计 10 604 254 455 B → 97 830 659 B（0.92%）。各 harness 合计比例：claude 0.30%、codex 0.89%、prime 0.46%、pi 4.4%、omp 5.7%、gemini 5.3%、qoder 7.1%、workbuddy 7.9%、dsh 9.5%、cursor 14.9%、commandcode 16.2%、crosery 18.4%、antigravity 23.9%、reasonix 24.5%、factory 49.0%。单个会话最差：cursor 213%（几 KB 的小会话，归档的会话行与 zstd 帧开销超过原文件）、factory 73%、crosery 49%、commandcode 48%、qwen 46%、antigravity 38%。
- 常驻守护进程（launchd `com.crosery.uniflo`，7311）全程未停止、未重启：`launchctl list` 显示运行中（pid 772），`/v1/health` ok。

### 未验证

- `SystemTrash`（真实系统回收站）没有在任何测试里调用过；macOS `NSFileManager` 方式下"放回原处"是否可用、Linux / Windows 回收站的行为均未验证。
- Linux / Windows 上的 `file_id`（inode）、硬链接判定与 FSEvents 以外的文件监听路径未实测。
