# session-cleanup 规格

用户在 Uniflo 中确认后，可以清理会话源文件来释放空间。清理前，Uniflo 会先写入一份精简归档。归档后的会话仍然可以列出、查看、检索，并计入用量统计。这是"只读 harness 数据"契约的例外，记录在 ADR-0006 中，同时更新 `AGENTS.md`；ADR-0006 也修订了 ADR-0004 中"网关只读"的条款。

## 可清理范围

- **由适配器声明**：只有一个文件对应一个会话的 JSONL/JSON 型 harness 才能声明清理目标，即会话主文件加上该会话独占的附属文件（如 Claude 的 `<id>/` 子代理目录）。其他 harness 返回"不支持清理"。
  - 首批支持：claude、qoder、qwen、codex、pi、omp、crosery、commandcode、prime、workbuddy、codebuddy、factory、gemini、antigravity、reasonix、grok、kiro、kimi、cursor（只清理 CLI 转录）、dsh。
  - 不支持：SQLite 型的 opencode、kilo、zcode、mimocode、minimax、hermes、copilot、devin、openclaw 新库；cline、roo、kodu；craft。
- **会话本身不满足以下条件时，不可清理**：
  - 状态为 work；
  - 有存活进程；
  - 是其他会话的子代理（父会话要连同整棵子树一起清理）；
  - 目标路径不在该 harness 的根目录内；
  - 目标是符号链接或共享硬链接；
  - 目标同时属于另一个会话。

## 计划与执行

- **生成计划**：`POST /v1/cleanup/plan`，请求体为 `{sessions: [key…]}` 或 `{q: "<搜索语法>"}`，返回计划：
  - `plan_id`，计划 10 分钟后过期；
  - 每个会话的 `eligible` 和不可清理的原因；
  - 目标文件及其大小、mtime、inode/文件 ID；
  - 可释放的字节数和预计的归档大小。
- **执行计划**：`POST /v1/cleanup/plans/{id}/execute`，逐个会话依次处理：
  1. 重新校验目标归属和文件戳；文件在生成计划之后有变化，则该会话失败，原因为"源文件已变化"。
  2. 读取源文件并计算 SHA-256。
  3. 写入精简归档，并把清单写入清理日志持久化。
  4. 把目标移入系统回收站。
  5. 记录墓碑。
  - 单个会话失败不影响其他会话；任何情况下都不回退为永久删除。
  - 返回每个会话的结果：`{key, status: archived|failed|skipped, reason, freed_bytes, archive_bytes}`。
- **精简归档**：写在 `<数据目录>/archive/<harness>/<id>.jsonl.zst`，内容是 zstd 压缩的归一 `Session` 与 `Event` JSONL。
  - 保留：用户、助手和思考的正文，每条最多 16 KB；工具名；截断到 2 KB 的参数；截断到 2 KB 的输出；全部 usage（含 `model`、`cost_usd`）；system 事件；元数据。
  - 丢弃：图片、base64 数据块等内嵌二进制。
  - 被截断的事件标记 `truncated: true`。
- **归档会话的表现**：
  - 列表与详情中 `Session.archived = true`（wire schema 新增的可选字段），状态为 idle。
  - 事件从归档读取。
  - 计入 usage-cost 的全部聚合；可被全文检索；搜索语法新增 `is:archived`，并支持取反。
- **恢复**：用户从系统回收站还原源文件后，源文件重新出现，以源文件为准，归档标记消失。归档文件本身保留，直到用户删除。
- **墓碑**：防止已清理的会话被重新扫描后以"源文件"身份出现在旧位置的残留副本中。只记录路径、key 和时间。
- **归档管理**：
  - `GET /v1/archive`：列出归档会话及其大小。
  - `DELETE /v1/archive/{key}`：永久删除归档记录，属于写操作。
- **CLI**：`uniflo clean <key…|--query <语法>> [--dry-run] [--yes]`。
  - 先打印计划；交互式终端中需要确认，非交互时必须带 `--yes`。
  - `uniflo archive [ls|rm <key>]`。

## 写接口安全边界

所有写接口（本规格的接口，以及 agent-access 的"打开终端"接口）都必须同时满足：

- Host 是回环名（与读接口相同）；
- 不带 Origin，或 Origin 为回环、`--cors-origin` 中允许的来源；
- 带请求头 `X-Uniflo-Write: 1`（自定义请求头会触发浏览器的 CORS 预检）；
- 配置了 token 时，请求带上 token。

不满足时返回 403。守护进程以 `--read-only` 启动时，全部写接口返回 403，并在 `/v1/health` 中给出 `read_only: true`。

## 回收站

- 通过 `trash` crate 移入系统回收站：macOS 为 Finder 废纸篓，Linux 遵循 freedesktop 规范，Windows 为回收站。
- 测试中注入一个移动到临时目录的回收站实现，**测试绝不触碰用户真实的回收站**。

### Scenario: 计划阶段的可清理判定
- GIVEN 以下几类合成会话：一个 claude 会话（带子代理目录）、一个 opencode 会话、一个状态为 work 的 codex 会话、一个目标为符号链接的 pi 会话、一个 claude 子代理会话
- WHEN 对它们一起请求 `POST /v1/cleanup/plan`
- THEN 只有 claude 父会话可清理，它的目标包含子代理目录
- AND 其余会话分别给出原因：不支持清理、会话运行中、目标是符号链接、需随父会话一起清理
- AND 计划中给出可释放的字节数和预计的归档大小

### Scenario: 执行清理并保留精简归档
- GIVEN 一个合成的 claude 会话，大小约 2 MB，含大段工具输出和一张 base64 图片；测试中注入的回收站目录
- WHEN 先生成计划，再执行
- THEN 源文件和子代理目录被移到注入的回收站目录，内容的 SHA-256 与日志中记录的一致
- AND 归档文件不超过源文件大小的 20%
- AND 结果中 `freed_bytes`、`archive_bytes` 与实际一致
- AND 清理日志在文件移动之前已经持久化

### Scenario: 归档会话继续可用
- GIVEN 上一个场景中已被归档的会话
- WHEN 请求会话列表、`/v1/sessions/{key}/events`、`/v1/search` 和 `/v1/usage`
- THEN 会话带 `archived: true`，事件来自归档，被截断的事件标记 `truncated`
- AND 全文检索能命中归档中的正文
- AND 用量统计中该会话的 token 与费用和清理前一致
- AND `is:archived` 只返回归档会话，`!is:archived` 不包含它们

### Scenario: 计划后源文件变化则拒绝
- GIVEN 已生成的计划
- WHEN 在执行前向源文件追加一行，然后执行
- THEN 该会话的结果为 failed，原因为"源文件已变化"，源文件不被移动，也不写归档
- AND 计划过期（超过 10 分钟）后执行返回 410

### Scenario: 从回收站还原后以源文件为准
- GIVEN 一个已归档的会话
- WHEN 把源文件从注入的回收站目录移回原位置
- THEN 30 秒内该会话以源文件身份出现，不再带 `archived` 标记，也不重复列出
- AND 归档文件仍在，`GET /v1/archive` 仍然能看到它

### Scenario: 写接口安全边界
- GIVEN 运行中的守护进程
- WHEN 依次发送以下 `POST /v1/cleanup/plan` 请求：带外站 Origin；缺少 `X-Uniflo-Write` 请求头；配置了 token 但不带 token；守护进程以 `--read-only` 启动时的合法请求
- THEN 全部返回 403
- AND 来自回环、满足全部条件的合法请求返回 200
- AND `/v1/health` 在只读模式下给出 `read_only: true`

### Scenario: CLI clean 与 archive
- GIVEN 合成数据
- WHEN 依次运行 `uniflo clean <key> --dry-run`、`uniflo clean <key> --yes`、`uniflo archive ls`、`uniflo archive rm <key>`
- THEN dry-run 只打印计划，不改动任何文件
- AND `--yes` 执行后的结果与 REST 一致
- AND `archive ls` 列出该会话及其大小
- AND `archive rm` 之后归档文件被删除，该会话从列表中消失
- AND 非交互环境下不带 `--yes` 执行时拒绝，退出码不为 0

### Scenario: 文档与契约
- GIVEN 本子任务的交付
- WHEN 检查文档
- THEN ADR-0006 记录清理的例外、写接口的安全边界以及对 ADR-0004 的修订
- AND `AGENTS.md` 的"只读 harness 数据"契约写明这一例外及其约束
- AND `docs/api.md`、`docs/schema.md`（`archived` 字段）、`docs/search.md`（`is:archived`）、`docs/adapters.md`（每个 harness 是否支持清理）均已更新
