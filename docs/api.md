# 网关 API

> `uniflo daemon` 暴露的 HTTP 接口：REST 拿快照，SSE / NDJSON / WebSocket 拿实时流。数据格式见 `docs/schema.md`。

状态：`current` · 更新：2026-10-09

默认地址 `http://127.0.0.1:7311`。读接口只用 `GET`（和 CORS 预检 `OPTIONS`）；写接口用 `POST` / `DELETE`，必须满足 [写接口](#写接口) 的全部条件。所有成功响应带 `x-uniflo-seq` 头：响应生成时的最新 `seq`，可作为随后订阅的 `since`。

## REST

| 路径 | 参数 | 返回 |
|---|---|---|
| `GET /` | — | 名称、版本、schema 版本、端点列表 |
| `GET /demo` | 页面参数见 `examples/web/index.html` 头注释 | 内置网页演示（单文件，无外部运行时依赖；内嵌副本 `crates/uniflo-gateway/src/index.html`） |
| `GET /v1/health` | — | `{ok, version, schema, seq, sessions, working, uptime_ms, update_available, latest_version, latest_prerelease, read_only}`；`read_only` 为 true 表示守护进程以 `--read-only` 启动，写接口全部 403 |
| `GET /v1/harnesses` | — | `Harness[]`：`{id, name, roots, sessions, working}` |
| `GET /v1/stats` | — | 索引与读取计数：`sessions`、`sources`、`bad_lines`、`unknown`、`read_errors`、`index_ms`…；`usage` 为后台用量索引进度 `{ready, done, total}`（关闭用量索引时为 `null`）；`update` 为守护进程最近一次 crates.io 检查结果 `{current, latest, available, latest_prerelease, checked_at, error}`（`latest`/`available` 只看正式版，`latest_prerelease` 仅为探测到的更新预发布版、供用户自行决定是否安装），未开启后台检查时为 `null`；`fts` 为全文索引状态 `FtsStatus`（`docs/schema.md#全文检索`），`--no-fts` 时为 `null` |
| `GET /v1/sessions` | `q` 查询（`docs/search.md`）、`limit`（默认 100）、`format=ndjson` | `Session[]`，有 `q` 时按相关度、否则按 `updated_at` 倒序 |
| `GET /v1/sessions/{key}` | — | `Session`；未知 key 返回 404 |
| `GET /v1/sessions/{key}/events` | `limit`（默认 200，最大 10000）、`before=<pos>`、`max_text`（默认 32768，0 不截断）、`format=ndjson` | `{session, events, next_before}`；`events` 按时间正序，翻更早一页传 `before=next_before`；`next_before` 为 `null` 当且仅当没有更早的事件 |
| `GET /v1/sessions/{key}/events?around=<id>` | `limit`（默认 200，最大 10000）、`max_text`、`format=ndjson` | 以事件 `id` 为中心的窗口 `{session, around, events, next_before}`：前后各约 `limit/2` 条，一侧不足时由另一侧补足；`next_before` 同上，可继续往前翻；会话里没有该 id 时 404 |
| `GET /v1/search` | `q`（必填，语法见 `docs/search.md#全文检索`）、`filter`（会话搜索语法）、`kinds`（逗号分隔的事件 kind）、`limit`（会话数，默认 20，最大 200）、`offset` | `SearchResponse`（`docs/schema.md#全文检索`）：按会话分组，每个会话最多 3 条命中；索引仍在构建时 `indexing: true` 并附 `progress`，结果只含已建部分。有短词（< 3 字符，含 `-` 排除的短词）时按时间倒序，从最近活动的会话往回扫，同时有 ≥ 3 字符的词时只扫它们在索引里的命中：凑够 `offset + limit` 个会话就停，此时带 `scanned_until`、`total` 为下限；超过 2 s 预算就返回已找到的部分，并带 `partial: true`。同分的会话按 `updated_at`、会话 key 排，翻页顺序固定。缺 `q` 或语法无效 400；以 `--no-fts` 启动或索引打不开时 503，`error` 说明原因 |
| `GET /v1/usage` | 见下表 | `UsageReport`：按一个维度聚合 token 与费用；参数不合法返回 400 |
| `GET /v1/sessions/{key}/usage` | — | `SessionUsageDetail`：每一步明细与按回合汇总；未知 key 返回 404 |
| `GET /v1/models` | `q` 查询 | `ModelUsage[]`：会话里出现过的模型、目录匹配结果（`exact` / `approx` / `none`）、价格段、上下文上限、步数与费用 |
| `GET /v1/pricing` | — | `PricingStatus`：`{source, fetched_at, stale, error, models, overrides, last_attempt, pending, sync_enabled}` |
| `GET /v1/archive` | — | `ArchiveList`：已清理会话的归档及其大小，见 [会话清理](#会话清理) |
| `GET /v1/sessions/{key}/resume` | — | `ResumeInfo`：在会话自己的 harness 里继续它的命令 `{key, harness, supported, argv, cwd, command, command_powershell, reason}`，只给命令、不执行；规则与支持的 harness 见 `docs/agents.md#恢复会话`；未知 key 404 |
| `POST /v1/sessions/{key}/open-terminal` | `terminal=terminal`（默认）\|`iterm`\|`ghostty` | 写接口。macOS 在新终端窗口切到 cwd 执行恢复命令，返回 `{opened, terminal, command, cwd}`；不支持恢复、目录不存在或终端未安装 422，终端名无效 400；其他平台 501 并附 `command`、`command_powershell` |
| `GET /v1/memory` | `cwd`（绝对路径，可用 `~`，可省） | `MemoryFile[]`：agent 记忆与指令文件 `{path, scope, harness, bytes, updated_at}`，范围见 `docs/agents.md#记忆与指令文件`；`cwd` 不是绝对路径 400 |
| `GET /v1/memory/file` | `path`（必填） | `MemoryContent`：上面能列出的文件之一及其内容（最多 256 KB，超出 `truncated: true`）；其他路径 403，不存在 404 |

`{key}` 需要 URL 编码（`claude%3A4f1c…`；key 里可能有 `/`，也必须编码）。类型定义见 `docs/schema.md#用量与价格接口的类型`、`docs/schema.md#agent-接入接口的类型`。

### /v1/usage 参数

| 参数 | 说明 |
|---|---|
| `group_by` | `harness`（默认）、`model`、`project`、`cwd`、`dir`、`day`、`hour`、`weekday`、`session` |
| `q` | 会话搜索语法过滤（`docs/search.md`），如 `h:claude in:~/work since:7d`；其中的 `since:` / `before:` 作用于**事件时间**，`in:~/…` 展开家目录 |
| `since` / `until` | 事件时间窗口 `[since, until)`：`30m`、`2h`、`7d`、`1w`（相对现在）、`YYYY-MM-DD`（`tz` 时区的零点）或 epoch 毫秒；与 `q` 里的窗口取交集；默认全部历史 |
| `tz` | `day` / `hour` / `weekday` 分桶与日期参数的时区：`local`（默认）、`UTC`、`+08:00`、IANA 名（`Asia/Shanghai`，读系统 zoneinfo） |
| `under` | 只统计 cwd 在该目录及其下的会话；`group_by=dir` 时返回它的直接子目录（`.` 行是目录本身） |
| `depth` | `group_by=dir` 的下钻层数，默认 1；不给 `under` 时从所有 cwd 的公共根算起 |
| `limit` | 只保留前 N 行，其余折叠成 `key="(other)"` 一行，合计仍等于 `totals` |
| `sort` | `cost`（维度默认）、`tokens`、`steps`、`sessions`、`prompts`、`key`（时间分桶默认） |

- `project` 是 cwd 向上找到的 git 根（含 `.git` 的目录），找不到时取 cwd 本身；只做 stat，结果缓存。
- `model` 分组按目录条目归并：精确匹配的别名（`anthropic/claude-sonnet-4.5`、带日期后缀的）并入目录 id；其余按去掉 `provider/` 前缀和修饰后的名字分组。没有模型的步骤归入 `key=""`。
- `prompts` 是非注入的用户输入数，按回合所用模型 / 输入时间计入对应行。
- `indexing.ready=false` 表示后台还没读完全部历史，结果会继续增长；`pricing.stale=true` 表示价格超过 24 小时没同步成功（只用内置快照也算）。
- 费用口径：`cost_usd` 只累加能定价的步；`unpriced_steps` 单独计数，不按 0 计。详见 `docs/decisions/ADR-0010-价格目录同步与费用口径.md`。

性能：用量索引在守护进程启动后后台建立，不阻塞 `/v1/health`；建立完成后聚合在内存里完成，本机真实数据（约 38 万步）每次请求在 0.1 s 内（`project` 首次需要 stat 各 cwd，约 1 s，之后缓存）。

## 会话清理

用户确认后释放会话源文件占用的空间：先写精简归档，再把源文件移入系统回收站，永不删除。决策与安全边界见 `docs/decisions/ADR-0006-会话清理与写接口.md`，类型见 `docs/schema.md#会话清理`。除 `GET /v1/archive` 外都是写接口。

| 路径 | 请求 | 返回 |
|---|---|---|
| `POST /v1/cleanup/plan` | `{"sessions": ["<key>", …]}` 或 `{"q": "<会话搜索语法>"}`（两者可同时给，取并集） | `CleanupPlan`：`plan_id`、`expires_at`（10 分钟后）、每个会话的 `eligible` / `reason` / `message`、要移走的目标（路径、大小、文件数、mtime、inode）、子会话 `children`、`freed_bytes` 与预计的 `archive_bytes`。只读元数据，不改任何东西；请求体不合法或为空 400 |
| `POST /v1/cleanup/plans/{id}/execute` | — | `CleanupReport`：按计划顺序逐个会话处理，每个会话 `{key, status: archived\|failed\|skipped, reason, message, freed_bytes, archive_bytes, children}`；一个会话失败不影响其他会话。未知或已执行过的计划 404，过期 410 |
| `GET /v1/archive` | — | `ArchiveList`：`{archives: [{key, harness, id, title, cwd, root, path, bytes, source, source_bytes, archived_at, restored}], bytes}`，按 `archived_at` 倒序 |
| `DELETE /v1/archive/{key}` | — | `ArchiveRemoved`：`{removed: [key…], bytes}`。永久删除归档文件；删除根会话时连同它的子会话归档、墓碑一起删除。源文件未还原的会话随之从列表消失（`removed` envelope）。没有该归档 404 |

- **可清理**：会话的 harness 声明了清理目标（`docs/adapters.md#会话清理`），且整棵子树（会话 + 子代理）都满足下表之外的条件。计划只针对请求的会话；子代理会话单独请求时不可清理，要随父会话一起清理。
- **执行**：对每个可清理的会话依次 ① 重新判定并比对目标的大小、mtime、inode，变化即失败（`source_changed`），不移动、不写归档；② 读取全部文件并算 SHA-256；③ 写归档 `<数据目录>/archive/<harness>/<id>.jsonl.zst`，清单（路径、大小、mtime、SHA-256）写进 `archive/cleanup.log.jsonl` 并落盘；④ 再比一次文件戳；⑤ 移入系统回收站；⑥ 记录墓碑。
- **之后**：会话带 `archived: true`、状态 idle，事件、全文检索、用量统计都从归档读取，`is:archived` 可筛选。从回收站把源文件还原到原位置后，30 秒内（通常 1 秒内）恢复为源文件，`archived` 消失，不重复列出；归档留到 `DELETE /v1/archive/{key}`，`GET /v1/archive` 中标 `restored: true`。
- **回收站**：macOS 废纸篓、Linux freedesktop、Windows 回收站（`trash` crate）。环境变量 `UNIFLO_TRASH_DIR=<目录>` 改为移动到该目录（测试与沙箱用）；设了 `UNIFLO_HOME` 而没设 `UNIFLO_TRASH_DIR` 时拒绝移动，结果为 `trash_failed`。
- 守护进程没有数据目录、打不开归档时，这几个接口返回 503。

`reason` 取值（`message` 是对应的中文说明，有细节时附在 `：` 之后）：

| reason | message | 何时 |
|---|---|---|
| `unknown_session` | 未知会话 | key 不存在 |
| `archived` | 已归档 | 已经清理过 |
| `subagent` | 需随父会话一起清理 | 是另一个会话的子代理 |
| `unsupported` | 不支持清理 | 该 harness（或子树里某个会话的 harness）没有声明清理目标 |
| `working` | 会话运行中 | 子树里有会话状态为 work |
| `live_process` | 有存活进程 | 子树里有会话附着着进程 |
| `source_missing` | 源文件不存在 | 源文件已不在 |
| `outside_root` | 目标不在 harness 根目录内 | 目标解析后不在该 harness 的根目录下 |
| `symlink` | 目标是符号链接 | 目标本身或目录里有符号链接 |
| `hardlink` | 目标是共享硬链接 | 目标文件的链接数大于 1 |
| `shared_target` | 目标同时属于另一个会话 | 目标里还有子树之外的会话的源 |
| `source_changed` | 源文件已变化 | 执行时文件戳、大小或子会话与计划不一致 |
| `archive_failed` | 写入归档失败 | 写归档、清理日志或索引失败；文件不动 |
| `trash_failed` | 移入回收站失败 | 第一个目标就没移走；归档撤销，文件不动 |
| `trash_partial` | 只有部分文件移入了回收站 | 移走了一部分；保留归档，未移走的文件继续作为源读取 |

## 实时流

三种传输共用同一组参数和同一份 `Envelope` 序列：

| 路径 | 传输 |
|---|---|
| `GET /v1/stream` | SSE：`id` = `seq`，`event` = `type`，`data` = Envelope JSON；支持 `Last-Event-ID` |
| `GET /v1/stream.ndjson` | 分块响应，每行一个 Envelope |
| `GET /v1/ws` | WebSocket，每帧一个 Envelope 文本 |

| 参数 | 说明 |
|---|---|
| `since=<seq>` | 先重放 `seq` 之后仍在重放环里的 Envelope；超出范围时先发 `lagged` |
| `session=<key,…>` | 只要这些会话 |
| `harness=<id,…>` | 只要这些 harness |
| `types=session,event,removed` | 只要这些 Envelope 类型（`hello`、`lagged` 总会发送） |
| `kinds=tool_call,assistant_message,…` | 只要这些事件 kind |
| `max_text=<n>` | 事件文本截断长度，默认 32768，0 不截断 |

推荐的客户端同步流程：

1. `GET /v1/sessions?limit=…` 渲染列表，记下 `x-uniflo-seq` 为 `S`。
2. 订阅 `/v1/stream?since=S`；`session` 按 key 替换，`event` 按 `(session, id)` upsert，`removed` 删除。
3. 打开某个会话时 `GET …/events` 拿最近一页，再用 `session=<key>` 过滤的流或总流增量更新。
4. 收到 `lagged`，或守护进程重启（`hello.seq` 小于已知 seq）时，回到第 1 步。

## 安全

| 规则 | 行为 |
|---|---|
| Host 校验 | 只接受 `localhost` / `127.0.0.1` / `::1` / `*.localhost`，以及 `--allow-host` 列出的值；否则 403（防 DNS rebinding） |
| Origin 校验 | 带 `Origin` 的请求只放行回环来源和 `--cors-origin` 列出的来源（`*` 放行全部）；否则 403 |
| Token | 配了 `--token` / `UNIFLO_TOKEN` 时，必须带 `Authorization: Bearer <token>` 或 `?token=<token>`（浏览器 EventSource/WebSocket 用后者）；否则读请求 401、写请求 403 |
| 写接口 | `GET` / `HEAD` / `OPTIONS` 以外的请求通过上面三项检查后，还要满足 [写接口](#写接口) 的全部条件，否则 403；`--read-only` 时一律 403 `read-only`。浏览器跨域发写请求要先过预检，CORS 放行 `GET, POST, DELETE, OPTIONS` 和 `x-uniflo-write` 头 |
| 只读 | 默认只读：唯一会改动 harness 文件的是用户确认的[会话清理](#会话清理)（移入回收站，不删除）；`--read-only` 关闭全部写接口。`uniflo setup` 改 harness 配置在本机命令行执行，不经网关；价格同步由守护进程定时或 `uniflo pricing sync` 在本机执行，不经网关 |

`--allow-host` 必须与 token 一起使用；对局域网暴露前确认用户授权。CORS 预检放行的方法为 `GET, POST, DELETE, OPTIONS`，请求头为 `authorization, last-event-id, content-type, x-uniflo-write`。

### 写接口

会改动任何东西的接口（目前是[会话清理](#会话清理)的 `POST /v1/cleanup/plan`、`POST /v1/cleanup/plans/{id}/execute`、`DELETE /v1/archive/{key}`）在上表之外还必须同时满足：

| 条件 | 说明 |
|---|---|
| Host 是回环名 | `localhost` / `127.0.0.1` / `::1` / `*.localhost`；`--allow-host` 只对读接口生效 |
| Origin | 不带，或为回环 http(s) 来源，或在 `--cors-origin` 中逐字列出；`*` 对写接口不生效 |
| `X-Uniflo-Write: 1` | 自定义请求头，浏览器必须先过 CORS 预检，外站页面无法用简单请求触发 |
| Token | 配置了 token 时必须携带 |

任一条件不满足返回 403，`error` 说明是哪一条。`uniflo daemon --read-only` 启动时全部写接口返回 403，`/v1/health` 的 `read_only` 为 true。实现是 `uniflo_gateway::write::check`，网关守卫对 `GET` / `HEAD` / `OPTIONS` 以外的每个请求都调用它，新写接口无需另接线，只要在本节登记；规则见 ADR-0006。目前的写接口：`POST /v1/cleanup/plan`、`POST /v1/cleanup/plans/{id}/execute`、`DELETE /v1/archive/{key}`、`POST /v1/sessions/{key}/open-terminal`。

## 例子

```sh
curl -s localhost:7311/v1/sessions?q='s:work'
curl -s "localhost:7311/v1/sessions/$(printf %s 'claude:4f1c' | jq -sRr @uri)/events?limit=50"
curl -N localhost:7311/v1/stream.ndjson?kinds=tool_call,tool_result
curl -s "localhost:7311/v1/usage?group_by=model&q=$(printf %s 'h:claude since:7d' | jq -sRr @uri)"
curl -s "localhost:7311/v1/usage?group_by=day&tz=Asia/Shanghai&since=30d"
curl -sG localhost:7311/v1/search --data-urlencode 'q=缓存击穿 -rollback' --data-urlencode 'filter=h:claude since:7d'
curl -s "localhost:7311/v1/sessions/claude%3A4f1c/events?around=msg_01:2&limit=20"   # 打开某条命中
curl -s -X POST -H 'X-Uniflo-Write: 1' -H 'Content-Type: application/json' localhost:7311/v1/cleanup/plan -d '{"q":"h:claude before:90d"}'
curl -s -X POST -H 'X-Uniflo-Write: 1' localhost:7311/v1/cleanup/plans/<plan_id>/execute   # 确认后执行
curl -s localhost:7311/v1/sessions/claude%3A4f1c/resume | jq -r .command                # 恢复命令
curl -s -X POST -H 'X-Uniflo-Write: 1' 'localhost:7311/v1/sessions/claude%3A4f1c/open-terminal?terminal=ghostty'
curl -sG localhost:7311/v1/memory --data-urlencode "cwd=$PWD"
```

```js
const es = new EventSource("http://127.0.0.1:7311/v1/stream?since=" + seq);
es.addEventListener("event", (m) => upsert(JSON.parse(m.data).event));
es.addEventListener("session", (m) => replace(JSON.parse(m.data).session));
```
