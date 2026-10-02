# Harness 适配

> 已支持的 harness、各自的存储与回合信号，以及新增一个 harness 的步骤。

状态：`current` · 更新：2026-10-02

## 已支持

| id | 名称 | 存储（`~` = 家目录） | 格式 | 回合结束信号 | 进程映射 |
|---|---|---|---|---|---|
| `claude` | Claude Code | `~/.claude/projects/<slug>/<id>.jsonl`，子代理 `<id>/subagents/**/agent-*.jsonl` | JSONL | `stop_reason` end_turn/stop_sequence、`turn_duration`、中断 | `~/.claude/sessions/<pid>.json` |
| `qoder` | Qoder | `~/.qoder{,-cn}/projects` | 同 Claude | 同 Claude | — |
| `qwen` | Qwen Work | `~/.qwenworkcn/projects` | 同 Claude | 同 Claude | — |
| `codex` | Codex | `~/.codex/{sessions,archived_sessions}/**/rollout-*.jsonl` | JSONL | `task_complete` / `turn_aborted` | —（app-server 一进程多会话） |
| `pi` | Pi | `~/.pi/agent/sessions/<slug>/<ts>_<id>.jsonl` | JSONL | `stopReason` | `ps` + `lsof` |
| `omp` | oh-my-pi | `~/.omp/agent/sessions/…` | 同 Pi | 同 Pi | `ps` + `lsof` |
| `crosery` | Crosery Agent | `~/.crosery/agent-sessions` | 同 Pi | 同 Pi | — |
| `commandcode` | Command Code | `~/.commandcode/projects` | 同 Pi（Anthropic 块） | 纯文本回复 | — |
| `gemini` | Gemini CLI | `~/.gemini/tmp/<project>/chats/session-*.jsonl`（旧版 `.json`） | JSONL，消息原地重写 | 无工具调用的回复 | — |
| `antigravity` | Antigravity | `~/.gemini/antigravity{,-cli}/brain/<id>/…/transcript.jsonl` | 步骤 JSONL | 无工具调用的规划步骤 `DONE` | — |
| `opencode` | OpenCode | `~/.local/share/opencode/opencode.db` | SQLite | 助手消息完成且 `finish != tool-calls` | — |
| `kilo` | Kilo Code | `~/.local/share/kilo/kilo.db` | 同 OpenCode | 同 OpenCode | — |
| `zcode` | ZCode | `~/.zcode/cli/db/db.sqlite` | 同 OpenCode | 同 OpenCode | — |
| `mimocode` | MiMo Code | `~/.local/share/mimocode/mimocode.db` | 同 OpenCode | 同 OpenCode | — |
| `workbuddy` | WorkBuddy | `~/.workbuddy/projects/<slug>/<id>.jsonl` | JSONL | 助手消息完成 | `~/.workbuddy/sessions/<pid>.json` |
| `minimax` | MiniMax Code | `~/.minimax/v2/sqlite/runtime-state.sqlite` | SQLite | `turn_ingress` completed/failed/aborted | — |
| `hermes` | Hermes | `~/.hermes/state.db` | SQLite | 终止型 `finish_reason` | — |
| `factory` | Factory Droid | `~/.factory/sessions/<slug>/<id>.jsonl` | JSONL | 纯文本回复 | — |
| `reasonix` | Reasonix | `~/.reasonix/projects/<slug>/sessions/*.events.jsonl` | 追加 / 替换日志 | 纯文本回复 | — |
| `cursor` | Cursor Agent | `~/.cursor/projects/**/agent-transcripts/<id>/<id>.jsonl` | JSONL（无 id、无时间） | 纯文本回复 | — |

每个模块文件头有格式细节与怪癖（`crates/uniflo-adapters/src/<模块>.rs`）。没有回合结束标记的 harness 依赖超时规则（`docs/architecture.md#状态机`）。

## 新增一个 harness

1. **调研格式**：只看结构不看内容。统计记录类型、字段名、文件布局，输出计数；不要把真实会话正文贴进对话、注释或测试。
2. **选接口**：
   - 一个文件 = 一个会话的 JSONL / JSON → 实现 `uniflo_core::LineDecoder`，用 `JsonlAdapter::new(...)` 包装，自动获得头尾摘要、跟随、分页、去重。
   - 数据库或一文件多会话 → 实现 `uniflo_core::Adapter`；SQLite 用 `crate::sqlite::open_ro`，跟随按 rowid，绝不全表扫描。
3. **映射**：每条原始记录 → `cx.emit(id, ts, Body)`。`id` 要在会话内稳定，同一条消息被原地更新时复用同一 id。工具调用与结果用同一个 `call_id` 配对。有显式回合边界时发 `TurnStart` / `TurnEnd`；不要在适配器里推断状态。
4. **元数据**：`cx.meta()` 填 `cwd`、`model`、`started_at`、`title`（带优先级：1 摘要 < 2 自动标题 < 3 用户命名）、`parent`。
5. **未知记录**：`cx.unknown("type=…")`；确认无用的类型加进模块的 `IGNORED`。
6. **注册**：`Cargo.toml` 加 feature 并放进 `default`；`lib.rs` 加 `#[cfg(feature = "…")] pub mod …;` 并在 `all()` 中注册。
7. **测试**：`common::testkit::Fixture` 写合成文件，至少断言一个完整回合的事件 kind 序列、最终状态、元数据、`unknown` 为空；有子代理/分叉时测 `identify()` 的 id 与 parent。
8. **真实数据验收**：`cargo build --release && ./target/release/uniflo scan --no-cache --json`，看该 harness 的会话数，以及 `stats.unknown` 为空、`bad_lines`、`read_errors` 为 0。
9. **进程映射（可选）**：harness 有 pid 注册表时实现 `live()` + `live_roots()`；没有时可用 `uniflo_core::procs::ProcCache` 探测进程，并优先用确定性证据（命令行参数、打开的文件）。
