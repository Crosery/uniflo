# fulltext-search 规格

Uniflo 为会话正文建立本地全文索引，支持中文与代码子串检索。命中结果可以直接定位到会话中的某一条事件。既有的元数据搜索（fd/fzf 语法，`docs/search.md`）保持不变；全文检索是一个新的、独立的接口。

## 索引

- **存储**：SQLite FTS5 trigram 索引，位于缓存目录（macOS 为 `~/Library/Caches/uniflo/fts-v1.sqlite`，其他平台用对应的缓存目录；`UNIFLO_HOME` 覆盖时随之变化）。
  - 使用依赖树中已有的 bundled `rusqlite`。
  - 这是 Uniflo 自己的派生数据，可随时删除；删除后重启即重建。
- **索引内容**，按事件逐条：
  - user_message、assistant_message 的正文；
  - reasoning 正文（`[redacted]` 不入索引）；
  - tool_call 的工具名，以及截断到 2 KB 的参数；
  - tool_result 截断到 4 KB 的输出；
  - system 的 subtype。
  - `partial` 事件按 id 覆盖，不产生重复。
- **构建方式**：
  - 首次全量构建在后台进行，不阻塞 REST 就绪。构建期间 `/v1/search` 返回已有部分，并附带 `indexing: true` 与进度。
  - 新事件在 2 秒内可被检索到。
  - 会话被移除时，其索引行一并删除。
- **格式版本**：索引带格式版本号，版本变化时自动重建；标签与 `index-v1.json` 一样包含 Uniflo 版本和 schema 版本。
- **关闭**：`uniflo daemon --no-fts` 关闭索引，此时 `/v1/search` 返回 503，并说明原因。

## 查询

- `GET /v1/search`
  - **参数**：
    - `q`：检索词。空白分隔的多个词按 AND 组合；`"..."` 表示短语；以 `-` 开头的词表示排除。
    - `filter`：复用会话搜索语法做过滤，例如 `h:claude in:~/work since:7d is:archived`。
    - `kinds`：限定事件类型。
    - `limit`、`offset`。
  - **返回**：按会话分组的命中结果。
    - 每个会话：`{session, title, harness, cwd, hits[], score}`。
    - 每条命中（每个会话最多 3 条）：`{event, kind, ts, snippet}`。`snippet` 中用 `\u0002` / `\u0003` 标记高亮区间。
  - **排序**：bm25 乘以近因加权，近因加权为 `1 + 1/(1 + 距今天数/30)`。
  - **短词**：长度小于 3 个字符的词改用 LIKE 子串匹配，结果按时间倒序。
- `GET /v1/sessions/{key}/events?around=<event-id>&limit=N`：返回以该事件为中心的事件窗口，用于从命中结果跳转到会话位置。
- **CLI**：`uniflo grep <检索词…> [--filter <语法>] [--limit N] [--json]`。可读输出为每个会话一行标题，下面附带片段。
- **对外契约**：响应类型在 `uniflo-schema` 中定义；`docs/api.md`、`docs/search.md` 和 ADR-0008（全文检索索引，修订 ADR-0001 的"不使用嵌入式数据库"）同步更新；ADR-0002 的"复议触发"一节注明本次触发。

### Scenario: 中文与代码子串命中
- GIVEN 合成会话，其中的事件包含以下内容：中文句子 "修复缓存击穿问题"、代码 `fn parse_releases(`、工具输出 "error[E0382]"、一条被截断前长度为 10 KB 的工具输出（关键词位于第 3 KB 处）
- WHEN 分别检索 "缓存击穿"、"parse_rel"、"E0382"
- THEN 每次都返回对应的会话和事件，片段中带高亮标记
- AND 位于第 3 KB 处的关键词能被检索到，因为它在 4 KB 截断范围内

### Scenario: 过滤、排除、短语与排序
- GIVEN 两个 harness 各有一个会话都包含 "deploy"，其中一个是 10 天前，另一个是今天
- WHEN 依次检索 `q=deploy`、`q=deploy&filter=h:<harness>`、`q="deploy script"`、`q=deploy -rollback`
- THEN 无过滤时今天的会话排在前面
- AND harness 过滤只返回对应的那个会话
- AND 短语检索只命中相邻出现的情况
- AND 排除词起作用

### Scenario: 增量更新与 partial 覆盖
- GIVEN 守护进程在运行，索引已建好
- WHEN 向会话文件追加一条包含新词 "zephyrquartz" 的消息；对同一 id 的 partial 事件先写入 "alpha"，再覆盖为 "alphabet"
- THEN 2 秒内检索 "zephyrquartz" 能命中
- AND 检索 "alphabet" 只命中 1 条
- AND 被覆盖前的 partial 内容不留下独立命中

### Scenario: 定位到事件窗口
- GIVEN 一条命中结果的 `event` id
- WHEN 请求 `GET /v1/sessions/{key}/events?around=<id>&limit=20`
- THEN 返回的窗口包含该事件，且该事件位于窗口中段，前后各约 10 条

### Scenario: 后台构建、格式升级与关闭
- GIVEN 一个首次启动的守护进程，合成数据有 2000 个事件
- WHEN 启动后立即请求 `/v1/health` 与 `/v1/search`
- THEN `/v1/health` 立即可用，`/v1/search` 返回 `indexing: true` 与进度
- AND 把索引格式版本号改大后重启，索引被重建
- AND 以 `--no-fts` 启动时 `/v1/search` 返回 503 并说明原因，且不创建索引文件

### Scenario: CLI grep
- GIVEN 同一份合成数据
- WHEN 运行 `uniflo grep 缓存击穿 --json` 并与 `GET /v1/search?q=缓存击穿` 比较
- THEN 两者的会话与事件 id 完全一致
- AND 不带 `--json` 时，按会话分组输出标题与片段，终端中有高亮

### Scenario: 本机真实数据性能
- GIVEN 本机真实会话数据
- WHEN 守护进程完成后台索引后，对 10 个中文词和 10 个代码子串各检索 5 次
- THEN p50 延迟小于 50 ms，p95 延迟小于 200 ms
- AND 记录以下数据：索引文件大小与源数据总量之比、全量构建耗时
- AND 输出中只包含计数与耗时，不包含片段正文
