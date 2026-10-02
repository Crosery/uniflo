# 会话搜索语法

> fd 式结构化过滤 + fzf 式模糊匹配，CLI（`uniflo ls/find`）与网关（`/v1/sessions?q=`）共用。实现在 `crates/uniflo-search`。

状态：`current` · 更新：2026-10-02

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
| `id:` | 会话 id 或 key 前缀 | `id:01a0` |
| `parent:` | 某会话的子会话 | `parent:claude:4f1c…` |

- 时间单位：`s`、`m`、`h`、`d`、`w`；绝对日期 `YYYY-MM-DD` 按 UTC 零点。
- 过滤器前加 `!` 取反：`!h:codex`、`!is:sub`。
- 模糊部分用 fzf 语法，匹配 标题、首条输入、cwd、harness、id：`gateway`、`'exact`、`^prefix`、`suffix$`、`!not`；智能大小写。
- 无模糊词时按 `updated_at` 倒序；有模糊词时按匹配分数，再按 `updated_at`。

```sh
uniflo ls 's:work'                   # 正在工作的会话
uniflo find "h:omp in:geek since:1d 'deploy"
uniflo ls --tsv -n 500 | fzf --with-nth 2.. --delimiter '\t' | cut -f1 | xargs uniflo tail -f
```
