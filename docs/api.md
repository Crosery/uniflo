# 网关 API

> `uniflo daemon` 暴露的 HTTP 接口：REST 拿快照，SSE / NDJSON / WebSocket 拿实时流。数据格式见 `docs/schema.md`。

状态：`current` · 更新：2026-10-09

默认地址 `http://127.0.0.1:7311`。只支持 `GET`（和 CORS 预检 `OPTIONS`）。所有成功响应带 `x-uniflo-seq` 头：响应生成时的最新 `seq`，可作为随后订阅的 `since`。

## REST

| 路径 | 参数 | 返回 |
|---|---|---|
| `GET /` | — | 名称、版本、schema 版本、端点列表 |
| `GET /demo` | 页面参数见 `examples/web/index.html` 头注释 | 内置网页演示（单文件，无外部运行时依赖；内嵌副本 `crates/uniflo-gateway/src/index.html`） |
| `GET /v1/health` | — | `{ok, version, schema, seq, sessions, working, uptime_ms, update_available, latest_version, latest_prerelease}` |
| `GET /v1/harnesses` | — | `Harness[]`：`{id, name, roots, sessions, working}` |
| `GET /v1/stats` | — | 索引与读取计数：`sessions`、`sources`、`bad_lines`、`unknown`、`read_errors`、`index_ms`…；`usage` 为后台用量索引进度 `{ready, done, total}`（关闭用量索引时为 `null`）；`update` 为守护进程最近一次 crates.io 检查结果 `{current, latest, available, latest_prerelease, checked_at, error}`（`latest`/`available` 只看正式版，`latest_prerelease` 仅为探测到的更新预发布版、供用户自行决定是否安装），未开启后台检查时为 `null` |
| `GET /v1/sessions` | `q` 查询（`docs/search.md`）、`limit`（默认 100）、`format=ndjson` | `Session[]`，有 `q` 时按相关度、否则按 `updated_at` 倒序 |
| `GET /v1/sessions/{key}` | — | `Session`；未知 key 返回 404 |
| `GET /v1/sessions/{key}/events` | `limit`（默认 200，最大 10000）、`before=<pos>`、`max_text`（默认 32768，0 不截断）、`format=ndjson` | `{session, events, next_before}`；`events` 按时间正序，翻更早一页传 `before=next_before`；`next_before` 为 `null` 当且仅当没有更早的事件 |

| `GET /v1/usage` | 见下表 | `UsageReport`：按一个维度聚合 token 与费用；参数不合法返回 400 |
| `GET /v1/sessions/{key}/usage` | — | `SessionUsageDetail`：每一步明细与按回合汇总；未知 key 返回 404 |
| `GET /v1/models` | `q` 查询 | `ModelUsage[]`：会话里出现过的模型、目录匹配结果（`exact` / `approx` / `none`）、价格段、上下文上限、步数与费用 |
| `GET /v1/pricing` | — | `PricingStatus`：`{source, fetched_at, stale, error, models, overrides, last_attempt, pending, sync_enabled}` |

`{key}` 需要 URL 编码（`claude%3A4f1c…`）。类型定义见 `docs/schema.md#用量与价格接口的类型`。

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
| Token | 配了 `--token` / `UNIFLO_TOKEN` 时，必须带 `Authorization: Bearer <token>` 或 `?token=<token>`（浏览器 EventSource/WebSocket 用后者）；否则 401 |
| 只读 | 网关没有任何写接口，不会修改 harness 数据；价格同步由守护进程定时或 `uniflo pricing sync` 在本机执行，不经网关 |

`--allow-host` 必须与 token 一起使用；对局域网暴露前确认用户授权。

## 例子

```sh
curl -s localhost:7311/v1/sessions?q='s:work'
curl -s "localhost:7311/v1/sessions/$(printf %s 'claude:4f1c' | jq -sRr @uri)/events?limit=50"
curl -N localhost:7311/v1/stream.ndjson?kinds=tool_call,tool_result
curl -s "localhost:7311/v1/usage?group_by=model&q=$(printf %s 'h:claude since:7d' | jq -sRr @uri)"
curl -s "localhost:7311/v1/usage?group_by=day&tz=Asia/Shanghai&since=30d"
```

```js
const es = new EventSource("http://127.0.0.1:7311/v1/stream?since=" + seq);
es.addEventListener("event", (m) => upsert(JSON.parse(m.data).event));
es.addEventListener("session", (m) => replace(JSON.parse(m.data).session));
```
