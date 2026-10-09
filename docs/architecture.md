# 架构

> 守护进程如何从各 harness 的会话存储得到统一、实时的会话与事件流。

状态：`current` · 更新：2026-10-08

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
   ▼
Gateway（axum）                       ← uniflo-gateway
   REST 快照 · SSE / NDJSON / WebSocket 实时流
   ▼
桌面端 / 网页端 / CLI（uniflo ls / tail / watch）
```

## 读取模型

- **一个源 = 一个文件或一个数据库**，源里可以有多个会话（SQLite）或一个会话（JSONL）。
- **冷启动只读头尾**：JSONL 源读开头 256 KB（元数据）+ 末尾 256 KB（最近状态），不解析全文；之后按字节偏移跟随。大文件历史通过反向逐行读取按 `before=pos` 分页。
- **跟随**：偏移之后的新字节逐行解码；半行留到下次；文件变短或被重写视为重置，重新做头尾摘要。一次追平上限 16 MB，超过改走摘要。
- **SQLite**：只读打开，按 rowid 读新行，未完成的 part/message 按主键重读；WAL 签名变化才触发读取。
- **同一 `id` 的事件是覆盖**（流式更新）：客户端按 `(session, id)` upsert。
- **缓存**：索引完成后、每 30 s（有变化时）和退出时把会话快照和游标写到 `~/Library/Caches/uniflo/index-v1.json`；启动时未变的源直接恢复，只读新增字节。

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
| WorkBuddy | `~/.workbuddy/sessions/<pid>.json` |
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
- 网页客户端：`examples/web/index.html` 是参考实现（快照 + `since` 续流、按 id upsert、工具结果并入调用卡片）；`crates/uniflo-gateway/src/index.html` 是内容相同的 cargo 包内副本，由网关编译进 `/demo`，修改时必须同步。
- 嵌入式使用：直接依赖 `uniflo-core` + `uniflo-adapters`，`Engine::new(all(), opts)` → `index()` → `run()`，无需网关。
