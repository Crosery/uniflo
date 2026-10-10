# 会话搜索语法

> 会话搜索：fd 式结构化过滤 + fzf 式模糊匹配，CLI（`uniflo ls/find`、`uniflo usage`）与网关（`/v1/sessions?q=`、`/v1/usage?q=`、`/v1/models?q=`）共用；全文检索：按事件正文查子串（`uniflo grep`、`/v1/search`）。实现都在 `crates/uniflo-search`。

状态：`current` · 更新：2026-10-09

空格分隔，全部条件同时成立。`key:value` 形式且 key 可识别的是过滤器，其余词拼成模糊查询。

| 过滤器 | 含义 | 例 |
|---|---|---|
| `h:` / `harness:` | harness id，逗号表示任一 | `h:claude,codex` |
| `s:` / `status:` / `is:` | 状态：`work`（`working`、`busy`）或 `idle` | `s:work` |
| `in:` / `cwd:` | cwd 包含（不区分大小写） | `in:uniflo` |
| `since:` / `after:` | `updated_at` ≥ 某时刻 | `since:2h`、`since:2026-10-01` |
| `before:` / `until:` | `updated_at` < 某时刻 | `before:7d` |
| `is:sub` / `is:root` | 子代理 / 顶层会话 | `is:root` |
| `is:live` | 有附着的进程 | `is:live s:work` |
| `is:archived` | 已清理、从归档读取的会话（`Session.archived`，`docs/api.md#会话清理`） | `is:archived h:claude`、`!is:archived` |
| `id:` | 会话 id 或 key 前缀 | `id:01a0` |
| `parent:` | 某会话的子会话 | `parent:claude:4f1c…` |

- 时间单位：`s`、`m`、`h`、`d`、`w`；绝对日期 `YYYY-MM-DD` 按 UTC 零点。
- 过滤器前加 `!` 取反：`!h:codex`、`!is:sub`、`!is:archived`（只看源文件还在的会话）。
- 模糊部分用 fzf 语法，匹配 标题、首条输入、cwd、harness、id：`gateway`、`'exact`、`^prefix`、`suffix$`、`!not`；智能大小写。
- 无模糊词时按 `updated_at` 倒序；有模糊词时按匹配分数，再按 `updated_at`。

## 用在用量统计里

`/v1/usage?q=`（以及 `uniflo usage [查询…]`）用同一套语法选会话，有两处不同：

- 未取反的 `since:` / `before:` 由 `Query::take_window()` 取出，作用于**每一步的事件时间**，而不是会话的 `updated_at`；与 `since=` / `until=` 参数取交集。取反的（`!since:…`）仍按 `updated_at` 过滤会话。
- `in:~/…` / `cwd:~/…` 先展开家目录再匹配。

```sh
uniflo ls 's:work'                   # 正在工作的会话
uniflo find "h:omp in:geek since:1d 'deploy"
uniflo ls --tsv -n 500 | fzf --with-nth 2.. --delimiter '\t' | cut -f1 | xargs uniflo tail -f
uniflo usage --by model h:claude in:~/work since:7d
```

## 全文检索

检索会话正文而不是元数据：`GET /v1/search?q=`（`docs/api.md`）、`uniflo grep`。索引是缓存目录里的 SQLite FTS5 trigram 索引 `fts-v1.sqlite`，守护进程在后台构建并跟随新事件，设计见 `docs/decisions/ADR-0008-全文检索索引.md`。

| 写法 | 含义 |
|---|---|
| `缓存击穿 parse_rel` | 空白分隔的词全部出现（AND），每个词按子串匹配，不区分大小写 |
| `"deploy script"` | 短语：引号内原样相邻出现 |
| `-rollback` | 排除含该词的事件；单独一个 `-` 按字面匹配 |

- 能搜到的内容：user / assistant 正文、reasoning（隐藏的 `[redacted]` 除外）、tool_call 的工具名和参数值（前 2 KB）、tool_result 输出（前 4 KB）、system 的 subtype。同一事件 id 只保留最新版本，流式 `partial` 的旧内容搜不到。
- `filter=` 就是上面的会话搜索语法（过滤器和模糊词都可以），只在匹配的会话里找：`filter=h:claude in:uniflo since:7d`。`kinds=` 限定事件 kind。
- 排序：按会话分组，分数 = bm25 × `1 + 1/(1 + 距今天数/30)`，天数按会话 `updated_at` 计；分数相同时 `updated_at` 新的在前，再按会话 key，所以翻页和重复查询的顺序固定。每个会话最多 3 条命中，最好的在前。
- 任一词（含 `-` 排除的词）少于 3 个字符（trigram 的下限，如 `ok`、`缓存`、`-fn`）时改为子串扫描，结果按时间倒序，`order` 为 `recent`、`score` 为 0。
  - 从最近活动的会话往回逐个扫；同时有 ≥ 3 字符的词时（如 `fn main`），只扫索引里含这些词的事件，再用短词过滤。凑够这一页的会话、且剩下的会话不可能有更新的命中时就停：响应带 `scanned_until`，`total` 只是下限，`uniflo grep` 显示为 `20 of 23+`。
  - 扫描最多 2 s。到时间就返回已找到的命中并带 `partial: true`，`uniflo grep` 会多打一行提示。罕见的短词可能因此找不全，加长成 ≥ 3 字符的词（`缓存击穿`）就能完整检索。
- `snippet` 是命中附近的一段文本，高亮区间用 `\u0002` … `\u0003` 包起来；换行已替换为空格。用命中的 `event` 调 `GET /v1/sessions/{key}/events?around=<event>` 打开上下文。
- 索引还在构建时结果不完整：响应带 `indexing: true` 和 `progress {done, total, events}`。
- 建完（或启动）后，守护进程在后台把索引合并成一个段，再把词项索引和文档长度表读进系统页缓存；期间 `/v1/stats` 的 `fts.warming` 为 true，某个词的第一次查询可能要几百毫秒。合并只在一次补建让索引分散到多个段之后发生（本机 3.6 GB 索引约 3 min），预读约 3 s。

```sh
uniflo grep 缓存击穿                         # 每个会话一行标题，下面是命中片段（终端里高亮）
uniflo grep 'parse_rel -test' --filter 'h:codex since:30d' -n 5
uniflo grep --json -- -rollback deploy      # 以 - 开头的参数放在 -- 之后；--json 输出与 /v1/search 相同
```

`uniflo grep` 优先问守护进程（`--url` / `UNIFLO_URL`）；连不上时在本进程打开同一份索引，先补齐增量（首次会全量构建，耗时见 ADR-0008）再查询。`uniflo daemon --no-fts` 不建索引，此时 `/v1/search` 返回 503。
