# 会话搜索语法

> 会话搜索：fd 式结构化过滤 + fzf 式模糊匹配，CLI（`uniflo ls/find`、`uniflo usage`）与网关（`/v1/sessions?q=`、`/v1/usage?q=`、`/v1/models?q=`）共用；全文检索：按事件正文查子串（`uniflo grep`、`/v1/search`）。实现都在 `crates/uniflo-search`。

状态：`current` · 更新：2026-10-26

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

检索会话正文而不是元数据：`GET /v1/search?q=`（`docs/api.md`）、`uniflo grep`。索引是缓存目录里的 SQLite FTS5 trigram 索引 `fts-v2.sqlite`，守护进程在后台构建并跟随新事件；基础设计见 `docs/decisions/ADR-0008-全文检索索引.md`，按会话年龄分热/冷两层存储的紧凑格式见 `docs/decisions/ADR-0011-全文索引紧凑格式.md`。

最近活动在 `--fts-hot-days`（默认 7，守护进程启动参数）天以内的会话整条留在热层（ADR-0008 的 position-aware trigram 格式，支持短语）；更老的会话整条压缩进冷层（zstd 压缩块 + 块级 trigram 索引，按需解压、在 Rust 里逐行校验与打分），一个会话永远只在一层，重新活跃时自动提升回热层。两层对查询透明：下面所有写法、排序、分页和子串扫描语义在热层、冷层和跨层结果里一致。

| 写法 | 含义 |
|---|---|
| `缓存击穿 parse_rel` | 空白分隔的词全部出现（AND），每个词按子串匹配，不区分大小写 |
| `"deploy script"` | 短语：引号内原样相邻出现 |
| `-rollback` | 排除含该词的事件；单独一个 `-` 按字面匹配 |

- 能搜到的内容：user / assistant 正文、reasoning（隐藏的 `[redacted]` 除外）、tool_call 的工具名和参数值（前 2 KB）、tool_result 输出（前 4 KB）、system 的 subtype。同一事件 id 只保留最新版本，流式 `partial` 的旧内容搜不到。
- `filter=` 就是上面的会话搜索语法（过滤器和模糊词都可以），只在匹配的会话里找：`filter=h:claude in:uniflo since:7d`。`kinds=` 限定事件 kind。
- 排序：按会话分组，热层分数 = bm25 × `1 + 1/(1 + 距今天数/30)`，天数按会话 `updated_at` 计；分数相同时 `updated_at` 新的在前，再按会话 key，所以翻页和重复查询的顺序固定。每个会话最多 3 条命中，最好的在前。bm25 的 idf 只按热层统计（热层为空时取 1.0），avgdl 则是热 + 冷两层的全局平均；这是冷层加入后唯一的排序语义变化，热层为空（`--fts-hot-days 0`）或全热（词只落在热层）时与 ADR-0008 的结果一致。
  - 相关性查询先把热层命中全部打分，再按 `updated_at` 从新到旧、多线程分批（每批每线程一个会话，最多 8 线程）解压冷层候选会话校验打分，每个会话只解压块级索引选中的候选块；候选会话分数的上界一旦确定低于当前这一页最差的分数，就停止继续解压——跳过的候选不计入 `scanned_until` 之外的误差：停下之后还会在约 250 ms 的预算内继续数剩下的候选，让 `total` 尽量精确；预算也用完时 `total` 变成下限，`scanned_until` 标出剩余部分从哪个 `updated_at` 开始。
  - 缓存：解压后的冷块按 LRU 留在内存（64 MB）；冷层块目录按冷层代号缓存；同一组词与 kind 过滤的冷层校验结果（每行的 id、长度、各词出现次数）留给后续翻页和重复查询，复用时按当前的 idf 与 avgdl 重新打分，结果与重新解压一致。任何冷层写入都会让后两者失效。
- 任一词（含 `-` 排除的词）少于 3 个字符（trigram 的下限，如 `ok`、`缓存`、`-fn`）时改为子串扫描，结果按时间倒序，`order` 为 `recent`、`score` 为 0；扫描会跨越热、冷两层（冷层会话解压后在 Rust 里做同样的子串匹配），对调用方透明。
  - 从最近活动的会话往回逐个扫，一页里可能同时含热层和冷层的会话；同时有 ≥ 3 字符的词时（如 `fn main`），热层只扫索引里含这些词的事件，冷层在解压后同样先用长词过滤。凑够这一页的会话、且剩下的会话不可能有更新的命中时就停：响应带 `scanned_until`，`total` 只是下限，`uniflo grep` 显示为 `20 of 23+`。
  - 扫描最多 2 s（`like_budget`）。到时间就返回已找到的命中并带 `partial: true`，`uniflo grep` 会多打一行提示。罕见的短词可能因此找不全，加长成 ≥ 3 字符的词（`缓存击穿`）就能完整检索。
- `snippet` 是命中附近的一段文本，高亮区间用 `\u0002` … `\u0003` 包起来；换行已替换为空格。用命中的 `event` 调 `GET /v1/sessions/{key}/events?around=<event>` 打开上下文。
- 索引还在构建时结果不完整：响应带 `indexing: true` 和 `progress {done, total, events}`。
  - `indexing` 只在有会话还没完成首次索引，或会话源文件变化后超过 15 s 仍未写入时为 true；已索引会话的实时追加不算积压，不会让它变回 true。
  - 同一轮构建内 `progress.done` 只增不减；新出现的会话只抬高 `total`。
- 建完（或启动）后，守护进程在后台把索引合并成一个段，再把词项索引和文档长度表读进系统页缓存；期间 `/v1/stats` 的 `fts.warming` 为 true，某个词的第一次查询可能要几百毫秒。合并只在一次补建让索引分散到多个段之后发生（本机 3.6 GB 索引约 3 min），预读约 3 s。
- 旁边留有旧版 `fts-v1.sqlite` 时，写线程在进入正常循环前先把它转换进 `fts-v2.sqlite`（按会话新旧顺序、每个会话一个事务，可中断续转），不重读会话源文件；守护进程照常启动，转换期间已转换的会话即可搜索，`/v1/stats` 的 `fts.migrating` 为 true。转换完删除旧文件，再按源文件刷新一遍所有会话（旧文件由旧版本写入），内容没变的冷会话只比较指纹、不重写。转换中途磁盘写满时也删除旧文件腾出空间，没转完的会话改从源文件补建。空闲轮次还会用 `PRAGMA incremental_vacuum` 回收压缩产生的空闲页，`fts.reclaimed_bytes` 记录迁移释放的字节数，重启后保留。

```sh
uniflo grep 缓存击穿                         # 每个会话一行标题，下面是命中片段（终端里高亮）
uniflo grep 'parse_rel -test' --filter 'h:codex since:30d' -n 5
uniflo grep --json -- -rollback deploy      # 以 - 开头的参数放在 -- 之后；--json 输出与 /v1/search 相同
```

`uniflo grep` 优先问守护进程（`--url` / `UNIFLO_URL`）；连不上时在本进程打开同一份索引，先补齐增量（首次会全量构建，耗时见 ADR-0008；已超过 `--fts-hot-days` 的会话直接写入冷层，不会先占热层再压缩）再查询。`uniflo daemon --no-fts` 不建索引，此时 `/v1/search` 返回 503。

同一缓存目录只允许一个新版进程写全文索引，其他进程读取已提交内容，写者退出后可自动接管。锁等待或缓存读取失败不会触发删除索引；不需要通过清空缓存修复锁竞争。升级后应让旧 MCP 连接重新启动，才能加载新版的跨进程协调与自动重连逻辑。

### 可靠性与外部写者

- **工作单元**：滚动写事务里，每个会话的一次写入（一页回填、一批实时事件、一条显式命令、一次降温/升温）是一个 SAVEPOINT。读完的冷层会话先攒成一组（最多 3000 行或 500 ms）再作为一个单元写入，这个单元失败时按会话逐个重试。单元失败只回滚它自己，记一次 `errors`，该会话按 0.5 s 起翻倍、最长 30 s 的间隔退避后重试；同一事务里其他单元照常提交，进行中的回填 pass 不会被整体丢弃。事务整体丢失（磁盘满、I/O 错误）时才重新对账。
- **两层互斥**：升温在一个单元内完成，失败时会话保持降温后的状态（冷标记与块都在）；升温遇到热层已有的事件行时保留热层那一行、跳过冷块里的同一事件，不报 `UNIQUE`。对账时发现同时处在两层的会话，就地升温合并（去重、不丢事件），之后由降温重新压回冷层。同一事件在两层各命中一次时只返回一条；命中的冷块行已不存在时回退到热层的行，找不到就跳过这条命中，查询不报错。
- **外部写者**：持有写租约的进程每次开写事务时比较 `PRAGMA data_version`。值变了说明别的进程（没有认写租约的旧版 `uniflo mcp`、手工打开的 SQL 终端）提交过，于是重新载入会话映射，约 1 s 内对账，把被外部删除或改动的会话重新排进索引，并让 `fts.foreign_writes` 加一。这类变化不计入 `errors`。
- **谁在写**：`fts.foreign_writers` 列出当前以写方式打开索引文件的其他进程 `{pid, command}`（`command` 只含可执行文件与子命令，不含参数值）。macOS 用 `lsof -F`，Linux 读 `/proc/<pid>/fd` 与 `fdinfo`，其他平台为空；只有见过外部写入（`foreign_writes` 大于 0）之后才去扫描，因为扫描要遍历本机所有进程，代价不小；结果在后台线程里刷新并缓存 10 s，不会拖慢 `/v1/stats`。看到旧版进程时重启它们即可，守护进程不会去结束别的进程。

