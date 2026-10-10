# usage-cost 规格

Uniflo 统计每次模型调用（一步）的 token 用量与费用，并对外提供以下接口：多维聚合、每步明细、模型目录。价格与模型信息来自内置快照和守护进程自动同步的上游目录。所有费用对订阅用户而言是"API 等价成本"，与真实账单不同。

## Usage 口径（wire schema，只增）

`usage` 事件字段的统一口径如下，写入 `docs/schema.md`：

| 字段 | 口径 |
|---|---|
| `input` | 未命中缓存的输入 token（不含 cache_read 和 cache_write） |
| `output` | 计费输出 token，**包含**思考 token |
| `reasoning` | `output` 中属于思考的部分，仅供参考，不重复计费 |
| `cache_read` | 命中缓存读取的 token |
| `cache_write` | 写入缓存的 token |
| `model`（新增，可选） | 这一步实际使用的模型 |
| `cost_usd`（新增，可选） | harness 自报的这一步金额，单位 USD |

- 对既有适配器逐个核对口径，不符合的修正：
  - OpenAI 形的 input 含缓存，要扣除缓存部分。
  - Gemini 的 `thoughts` 要计入 output。
  - 没有自报金额的 harness 不填 `cost_usd`。
- 补读 harness 自带的金额：
  - OpenCode 系：消息 `cost`。
  - pi 系：`usage.cost.total`。
  - Cline 系：`api_req_started.cost`。
  - Hermes：会话成本列与 token 列，作为会话级 usage。
  - Codex：`token_usage_record` 中按 turn 的权威用量；有它时不重复计 `token_count`。
- 适配器没有给出步骤 `model` 时，由引擎用该会话最近一条 assistant 消息的模型补上。

## 模型目录与价格

- **条目内容**：每个模型条目包含：
  - `id`、`provider`；
  - 价格段数组 `{from, until?, input, output, cache_read, cache_write?, tiers?[{above, input, output, cache_read, cache_write?}]}`，单位 USD / 百万 token；
  - `context_limit`、`output_limit`。
- **内置快照**：由 `scripts/build-pricing.mjs` 生成并入库。主源是 models.dev 的 `api.json`；models.dev 缺的条目用 LiteLLM 的 `model_prices_and_context_window.json` 补上。只取官方厂商的价格，不取转售商。
- **用户覆盖文件**：`<数据目录>/pricing/overrides.json`，同名条目优先于目录，任何同步都不会改它。
- **模型名归一**依次尝试：
  1. 去掉 `provider/` 前缀、`[1m]` 等后缀、末尾的 `(high)` 之类修饰；
  2. 大小写不敏感，`4.5` 与 `4-5` 视为相同；
  3. 去掉日期后缀，例如 `-20250929`；
  4. 以上都找不到时，取同骨架的最近版本，标记为 `approx`。
- 匹配结果为 `exact`、`approx` 或 `none` 之一。

## 价格同步

- **触发**：守护进程启动后约 1 分钟同步一次，之后每 6 小时一次，用系统 `curl -fsS --max-time 20` 拉取。
  - 源地址可用 `UNIFLO_PRICING_URL` / `UNIFLO_PRICING_FALLBACK_URL` 覆盖，供测试使用。
  - `--no-price-sync` 完全关闭同步。
- **写入**：成功后原子写入 `<数据目录>/pricing/catalog.json`，即先写临时文件再重命名。
- **新增价格段**：某个模型的价格与上次观测不同时，从**观测时刻**起新增一段价格，此前的用量仍按旧价段计算。
  - 任一分项变化超过 50% 时，需要连续两次读取一致才生效。
  - 缺失值或 0 不视为降价。
- **失败处理**：任一来源失败或返回空时，保留上次的数据并记录错误；超过 24 小时没有成功同步，状态标为 `stale`。
- **状态接口**：`GET /v1/pricing` 返回 `{source, fetched_at, stale, error, models, overrides}`，即同步状态和条目数。

## 费用计算

- 一步的费用按以下顺序决定：
  1. 有 harness 自报的 `cost_usd` 时直接采用，`cost_source=harness`；
  2. 否则按事件时间所在的价格段计算，`cost_source` 为 `catalog` 或 `approx`；
  3. 都没有时 `cost=null`，计入 `unpriced_steps`。
- 计算公式：`(input×in + output×out + cache_read×cr + cache_write×cw) / 1e6`。
  - `cw` 缺失时按 `in × 1.25` 计。
  - 分档按整单 prompt token 判定：`input + cache_read + cache_write` 超过 `above` 时，整单按该档价格计。
- 费用由价格段和事件时间唯一决定，所以重启重建索引后结果不变。

## 会话合计（wire schema，只增）

- `Session` 新增可选字段 `usage`，内容为：
  - `steps`；
  - `input`、`output`、`cache_read`、`cache_write`、`reasoning`；
  - `cost_usd`、`unpriced_steps`；
  - `last_context_tokens`、`context_limit`。
- `last_context_tokens` 是最后一步的 `input + cache_read + cache_write`；`context_limit` 来自模型目录。
- 有新的 usage 事件时，这些字段实时更新，并通过 `session` envelope 推送。

## 接口

- `GET /v1/usage`：多维聚合。
  - 参数：
    - `group_by`：`harness`、`model`、`project`、`cwd`、`dir`、`day`、`hour`、`weekday`、`session` 之一。
    - `q`：复用会话搜索语法做过滤，例如 `h:claude in:~/work since:7d`。
    - `since` / `until`：用于事件时间窗口。
    - `tz`：日、小时、星期分桶使用的时区，默认本地时区。
    - `under` 和 `depth`：用于目录树下钻。
    - `limit`、`sort`。
  - 每行返回：`{key, label, sessions, steps, prompts, input, output, cache_read, cache_write, reasoning, cost_usd, unpriced_steps}`。另外返回 `totals` 和 `pricing{fetched_at, stale}`。
  - `project` 是 cwd 向上查找到的 git 根，找不到时取 cwd 本身。只做 stat，结果会缓存。
  - `dir` 按 cwd 路径前缀聚合：
    - 给了 `under` 时，返回其直接子目录；
    - 不给 `under` 时，从所有 cwd 的公共根开始，按 `depth` 截断。
  - 各行合计等于 `totals`。
  - 默认的时间窗口是全部历史。
- `GET /v1/sessions/{key}/usage`：返回每一步的明细和按回合汇总。
  - 每步：`{event, ts, model, input, output, cache_read, cache_write, reasoning, cost_usd, cost_source, context_tokens, context_limit, context_pct}`。
  - 按回合：每回合的 token 与费用合计。
- `GET /v1/models`：列出会话中出现过的模型，每个模型带 `{model, match, catalog_id, prices, context_limit, output_limit, sessions, steps, cost_usd}`。
- **CLI**：
  - `uniflo usage [--by <group>] [--since 7d] [--under <dir>] [查询…] [--json]`：表格输出，与 REST 数值一致。
  - `uniflo usage <session-key> [--json]`：每步明细。
  - `uniflo pricing [--json]`：同步状态。
  - `uniflo pricing sync`：立即同步，不受 6 小时间隔限制。
- 实现放在库 crate 中（core，或新建一个只依赖 schema 的 crate，新增的依赖边写进 `docs/architecture.md`），嵌入式使用方同样可用。
- 新的响应类型在 `uniflo-schema` 中定义。
- `docs/api.md`、`docs/schema.md`、ADR-0010（价格目录同步与费用口径）要同步更新。

### Scenario: usage 口径归一
- GIVEN 四段合成 usage：Anthropic 形（input、cache_read、cache_creation 三项并列）、OpenAI / Codex 形（input 含 cached，reasoning 包含在 output 里）、Gemini 形（input 含 cached，thoughts 与 output 分开）、OpenAI completions 形（prompt 含缓存）
- WHEN 经过对应的适配器
- THEN 产生的 usage 事件都满足：`input` 不含缓存，`output` 含思考，`reasoning` 不超过 `output`
- AND 既有适配器的单元测试断言与新口径一致

### Scenario: 费用计算与来源优先级
- GIVEN 一个测试价格目录：模型 M 的 input 3、output 15、cache_read 0.3，没有 cache_write 价，另有一档 `above=200000`
- WHEN 计算以下几步的费用：普通的一步；prompt 超过 20 万 token 的一步；自带 `cost_usd=0.5` 的一步；模型未知的一步
- THEN 普通一步的费用与手算结果一致，误差小于 1e-9，cache_write 按 input×1.25 计价
- AND 超档的一步整单按档内价格计
- AND 自报的一步取 0.5，`cost_source=harness`
- AND 未知模型的一步 `cost_usd` 为 null，计入 `unpriced_steps`，不计作 0

### Scenario: 模型名归一与近似匹配
- GIVEN 测试目录中只有 `claude-sonnet-4-5` 和 `glm-5.2`
- WHEN 匹配 `anthropic/claude-sonnet-4.5`、`claude-sonnet-4-5-20250929`、`claude-sonnet-4-5[1m]`、`glm-5.3`、`totally-unknown`
- THEN 前三个的结果是 `exact`，指向 `claude-sonnet-4-5`
- AND `glm-5.3` 的结果是 `approx`，指向 `glm-5.2`
- AND `totally-unknown` 的结果是 `none`

### Scenario: 价格同步成功并按时间段生效
- GIVEN 守护进程使用一个本地模拟的价格源（通过环境变量覆盖 URL）；已有一个会话，含 T0 时刻的一步
- WHEN 价格源把模型 M 的 output 价改了 10%，触发 `uniflo pricing sync`，之后会话在 T1 又追加一步
- THEN `catalog.json` 被原子替换，M 新增一个 `from` 等于观测时刻的价格段
- AND T0 那一步的费用不变，T1 那一步按新价计算
- AND `GET /v1/pricing` 显示新的 `fetched_at`，`stale=false`

### Scenario: 同步失败与安全阀
- GIVEN 本地模拟的价格源
- WHEN 依次发生三件事：价格源返回 500；价格源把某模型价格改了 80%，只返回一次；价格源连续两次返回同样的 80% 改价
- THEN 返回 500 时保留旧目录，`error` 有值
- AND 只出现一次的 80% 改价不生效，连续两次之后才生效
- AND `overrides.json` 中的条目始终优先，同步不会改动该文件
- AND 以 `--no-price-sync` 启动时，整个过程中不发出任何价格请求

### Scenario: 多维聚合与目录树
- GIVEN 合成数据：两个 harness、三个模型、两个 git 仓库（其中一个仓库下有两个子目录 cwd）、跨三天的 usage
- WHEN 依次请求 `GET /v1/usage`，`group_by` 取 harness、model、project、cwd、day、hour、weekday、session，另外请求一次 `group_by=dir&under=<仓库根>`
- THEN 每种分组下各行的 token 与费用之和等于 `totals`
- AND project 分组把同一仓库下的两个子目录合并为一行
- AND `dir` 分组的 under 结果给出仓库根的直接子目录及各自的合计
- AND `q=h:<harness> since:1d` 只统计符合条件的部分
- AND `tz` 参数改变日期分桶的边界

### Scenario: 每步用量与上下文占用
- GIVEN 合成会话：两个回合共四步，每步带不同的 input 与 cache_read，模型的 `context_limit=200000`
- WHEN 请求 `GET /v1/sessions/{key}/usage`
- THEN 返回四步明细，每步的 `context_tokens = input + cache_read + cache_write`，`context_pct` 按 context_limit 计算
- AND 按回合汇总的结果等于对应各步之和
- AND 会话的 `usage.last_context_tokens` 等于最后一步的 context_tokens

### Scenario: 会话合计实时推送
- GIVEN 守护进程运行中，客户端订阅了 `/v1/stream`
- WHEN 向一个合成会话文件追加一条 usage 记录
- THEN 2 秒内客户端收到该会话的 `session` envelope，其中 `usage.steps` 加 1，`cost_usd` 已更新

### Scenario: CLI 与 REST 一致
- GIVEN 同一份合成数据
- WHEN 运行 `uniflo usage --by model --json` 与 `GET /v1/usage?group_by=model`，再运行 `uniflo usage <key> --json` 与 `GET /v1/sessions/{key}/usage`
- THEN 两两对比，各数值完全一致
- AND 不带 `--json` 时输出可读表格，含费用与未定价步数列

### Scenario: 本机真实数据统计
- GIVEN 本机真实会话数据，以及内置的价格快照
- WHEN 以 release 构建运行 `uniflo usage --by harness --json` 与 `uniflo usage --by model --json`
- THEN 正常返回，只输出各组的计数与金额合计，不输出正文
- AND 列出未定价模型及其步数
- AND 有缓存时守护进程启动到 `/v1/health` 可用的耗时，相比改动前增加不超过 50%
