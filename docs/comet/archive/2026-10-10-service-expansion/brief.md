# 目标

把 Uniflo 从"本机会话实时网关"扩展成**方便外部服务与 agent 接入的本地会话数据中枢**。核心仍是 cargo 包提供的对外接口（REST / SSE / NDJSON / WS + 可嵌入的库 crate），新能力同时通过 CLI、MCP、Skill 和网页演示提供。补齐 Wake 已验证有价值、而 Uniflo 缺失的能力，并利用常驻实时的优势覆盖 Wake 的结构性痛点：没有实时状态、没有结构化 API、没有成本统计、索引依赖桌面进程。

用户原话要点（2026-10-09）：
- 核心定位：方便用户接入外来服务。作为 cargo 包，对外接口是核心，这一点不变。
- Wake 可借鉴的五项全部适配：补适配器、MCP、恢复会话、正文全文检索、预编译分发。分析 Wake 的痛点，扩大我们的服务。
- 参考 `/Users/crosery/work_file/crosery-api-console` 的计费与自动更新同步。
- 用户清理会话时，Uniflo 帮忙保留占用更小的记录。
- 统计花费和 token，按会话、项目、目录及其他维度细分，并且更细。
- 额度：从上游看模型价格和相关信息，统计我们监控的会话用量即可；每一步的用量都要统计输入、输出、缓存等。
- 各 agent harness 有对应图标。
- demo 页面做好费用查询、使用额度、会话管理，参考别人设计的优点并规避其痛点。
- 安装与更新时询问是否全局安装 MCP 和/或 Skill，默认 MCP，由用户自己选择；这些服务要让 agent 查询和调用尽量简单。

# 范围

按能力划分，每项对应 `specs/<capability>/spec.md`，规格写明完整行为，Scenario 即详细验收项：

1. **harness-adapters**：
   - 新增 grok、kiro、kimi、copilot、openclaw、codebuddy、craft、devin 八个适配器。
   - cursor 补读 IDE `state.vscdb` 的元数据。
2. **usage-cost**：
   - 统一 usage 口径，每步带模型与 harness 自报金额。
   - 价格与模型目录：内置快照，守护进程自动同步。
   - 费用计算。
   - 会话合计。
   - 多维聚合：会话、项目、目录树、cwd、harness、模型、日、小时、星期。
   - 每步用量与上下文窗口占用。
   - CLI 命令。
3. **fulltext-search**：消息正文全文检索（中文与代码子串），命中结果可定位到事件；提供 REST 接口和 CLI 命令。
4. **session-cleanup**：
   - 先生成清理计划，再确认执行。
   - 执行时先写入精简归档，再把源文件移入系统回收站。
   - 归档会话仍可列出、查看、检索、计费。
   - 写接口有安全边界。
5. **agent-access**：
   - `uniflo mcp`：stdio MCP，返回结构化 JSON。
   - Skill。
   - `uniflo setup`：全局接入，安装和更新时询问。
   - 恢复会话：给出命令、`uniflo resume`、在 macOS 上直接打开终端。
   - agent 记忆与指令文件目录。
   - 接力上下文 `uniflo context`。
6. **distribution**：GitHub Releases 预编译二进制、安装脚本、cargo-binstall 元数据、按安装方式升级。
7. **web-console**：网页演示重做，内容包括：
   - harness 图标
   - 用量与费用面板
   - 每步用量
   - 全文检索视图
   - 会话管理（清理、归档、恢复）
   - 活跃度洞察
   - 回合结束通知

# 非目标

- 不向 agent 发送消息，不控制 agent 运行（Wake #4 的"应用内聊天"不做）。
- 不读取任何凭据：harness 登录文件、OAuth、钥匙串、`*.key`。不接 Claude / ChatGPT 订阅额度接口，不做外部额度提供方，不透传 Codex `rate_limits`。
- 不修改 SQLite 型或多会话共用文件型 harness 的数据，例如 OpenCode、Hermes、MiniMax、Devin、Copilot、Cline 系的 JSON 数组。这些 harness 不支持清理。
- 不改变既有 wire schema 字段的语义：只增加可选字段和新取值（ADR-0003）。`usage` 各字段原本没有定义口径，本次只补口径定义。
- 不做 Homebrew tap，不做 macOS 公证或签名，不做多机聚合（Wake #21），不按 harness 或项目开关索引（Wake #8）。
- Uniflo 自身不在后台自动安装升级。仍然是"检查 → 提示 → 手动 `uniflo update`"。
- Linux / Windows 的"直接打开终端"不实现，退回复制命令。
- 不做跨 harness 去重：Craft 驱动的 Claude 引擎副本仍会作为 claude 会话出现，在文档中注明。

# 验收示例

- harness-adapters 子任务：`specs/harness-adapters/spec.md` 的全部 Scenario 通过。本机真实数据跑 `uniflo scan --no-cache --json`：`grok` 会话数等于本机 `~/.grok/sessions` 下非空会话数；全部 harness 的 `stats.unknown` 为空，`bad_lines` 与 `read_errors` 为 0。
- usage-cost 子任务：`specs/usage-cost/spec.md` 的全部 Scenario 通过。在本机真实数据上，`GET /v1/usage?group_by=model` 与 `uniflo usage --by model --json` 给出相同的各模型 token 与费用合计；未定价模型单独计数，不记为 0。
- fulltext-search 子任务：`specs/fulltext-search/spec.md` 的全部 Scenario 通过。在本机真实数据上，索引在后台建完后，中文词与代码子串查询的 p50 延迟小于 50 ms。
- session-cleanup 子任务：`specs/session-cleanup/spec.md` 的全部 Scenario 通过。合成会话经"计划 → 确认"后，源文件进入回收站（测试中用注入的回收站目录），归档文件不大于源文件的 20%，归档会话仍出现在列表、检索与用量统计中。
- agent-access 子任务：`specs/agent-access/spec.md` 的全部 Scenario 通过。在隔离 HOME 中用真实 Claude Code 与 Codex 完成 `uniflo setup --mcp` 后，`claude mcp list` 显示 `uniflo` 已连接；对同一查询，REST、CLI `--json` 与 MCP `structuredContent` 返回的数字一致。
- distribution 子任务：`specs/distribution/spec.md` 的全部 Scenario 通过。用本机构建的当前平台发布包和本地模拟的发布服务器，`install.sh` 完成下载、校验、安装；校验值不符时中止且不留下文件。
- web-console 子任务：`specs/web-console/spec.md` 的全部 Scenario 通过。真实浏览器在合成数据上完成用量、检索、管理、洞察、通知各视图的 e2e，并留存明暗主题的宽屏与窄屏截图证据。

# 约束与不变量

- **只读 harness 会话数据。** 只有两个例外，各写 ADR，并同步更新 `AGENTS.md` 的契约：
  - 用户在 Uniflo 中逐次确认的会话清理：移入系统回收站，先写归档（ADR-0006）。
  - `uniflo setup` 对 harness **配置文件**的写入：写前备份，只动 `uniflo` 条目或软链（ADR-0007）。
- **不读取凭据。** 外部服务的 token 只走环境变量。
- **网关默认只听回环。** Host / Origin 校验不放宽（ADR-0004）。新增写接口必须同时满足四个条件，`--read-only` 可整体关闭写接口；ADR-0006 修订 ADR-0004 中"网关只读"一条：
  - 请求来自回环；
  - 不带 Origin，或 Origin 为回环或已允许的来源；
  - 带 `X-Uniflo-Write: 1` 请求头；
  - 配置了 token 时同时带 token。
- **wire schema 只增不改。** 改 schema 时在同一提交里同步 `docs/schema.md`。新增的 API 响应类型放在 `uniflo-schema`，作为对外契约，也方便 Rust 使用方直接依赖。
- **依赖方向单向**：`schema ← core ← search / adapters ← gateway ← cli`。新 crate 或新的依赖边写进 `docs/architecture.md`。新能力实现在库 crate 中，嵌入式使用方同样可用；网关和 CLI 只做接线。
- **生产依赖**：批准新增 `trash`、`toml_edit`、`sha2`，以及 `serde_json` 的 `preserve_order` feature（D13）。再要新增依赖须重新请示。MCP 协议手写。zstd 编码用已有的 `ruzstd`。FTS 复用已有的 bundled `rusqlite`。
- **测试数据**：测试夹具只用手写合成数据。调试真实数据时只打印计数、键名和金额合计，不打印正文。
- **对外联网只有两处，均走系统 `curl`，均可关闭，且不携带会话内容或标识符**：
  - 版本检查（既有），`--no-update-check` 关闭；
  - 价格目录同步（新增），`--no-price-sync` 关闭。
- **网页演示**保持单文件、免构建、可离线。`examples/web/index.html` 与 `crates/uniflo-gateway/src/index.html` 保持同步。第三方资源（图标、库）的许可证要内嵌或写入 `THIRD_PARTY_NOTICES.md`。
- **ADR 编号预先分配**，避免并行冲突：
  - 0006 会话清理与写接口
  - 0007 setup 写 harness 配置
  - 0008 全文检索索引
  - 0009 预编译分发与自升级
  - 0010 价格目录同步与费用口径
- **worklog**：每个子任务写自己的 `notes/2026-10-09-worklog-<child>.md`，记录命令与真实结果。集成时汇总到当天的 worklog。

# 决策

- D1（用户）：用新 worktree，分支 `task/service_expansion`，基于 stage。开工前已把 10-08 未提交的 demo 工作单独提交（`fbc78d4`）。
- D2（用户）：Wake 可借鉴的五项全部做：补适配器、MCP、恢复会话、正文全文检索、预编译分发。
- D3（用户，Q8=A）：安装和更新时询问接入方式，四选一：MCP（默认）/ Skill / 两者 / 跳过；再从检测到的 harness 中多选，默认全选。选择会被记住：之后只对新检测到的 harness 再询问，已接入的静默刷新。非交互环境下不写文件，只打印命令。
- D4（调查）：`cargo install` 没有安装后钩子。询问由 `uniflo setup` 承担，以下场景触发它：安装脚本结尾、`uniflo update` 结尾、首次在终端运行任意 uniflo 命令（只问一次）。守护进程从不提问。
- D5（调查）：MCP 采用 stdio 子进程 `uniflo mcp`，优先连本机守护进程，连不上时退回进程内索引。
- D6（调查）：Skill 真源放在 `~/.agents/skills/uniflo/`，只为不读该目录的 harness 建相对软链。有官方 `mcp add` CLI 的 harness 走 CLI：claude、codex、gemini、droid、codebuddy、qoder；其余直接编辑 JSON 或 TOML。
- D7（调查，D18 细化）：价格目录沿用 console 的结构与纪律：
  - 单位为 USD / 百万 token，价格按 `from/until` 时间段生效，按整单分档；
  - 两种缓存口径归一；
  - 未知模型记为"未定价"，不记 0；
  - 单个来源失败时保留旧数据；
  - 改价超过 50% 需两次读取一致才生效；
  - 用户覆盖永不被同步覆盖。
- D8（调查）：图标用 lobe-icons（MIT）的单色 SVG 内联，随深浅主题着色，没有图标时用字母块。网关以接口形式把图标提供给外部客户端。
- D9（调查）：本机没有数据的 harness（kiro、copilot、openclaw、codebuddy、craft、devin，以及 kimi 的对话记录）只能用合成夹具验证，交付时标注"未经真机验证"。grok 和 cursor IDE 元数据用本机真实数据做计数验收。
- D10（用户，Q1=A）：清理 = 先写精简归档，再把源文件移入系统回收站。
  - 归档内容：zstd 压缩的归一事件，保留正文、工具名、截断后的参数与输出、usage、费用、元数据；丢弃图片和大段输出。
  - 只支持一个文件对应一个会话的 harness。
- D11（用户，Q2=A）：网关新增写接口，安全边界见约束一节。清理分两步：生成计划，再确认执行。
- D12（用户，Q3=A）：全文检索使用缓存目录下的 SQLite FTS5 trigram 索引，可以用 `--no-fts` 关闭，也可以随时删除后重建。
- D13（用户，Q5=A）：批准新增依赖 `trash`、`toml_edit`、`sha2` 和 `serde_json/preserve_order`。
- D14（用户，Q6=A）：分发方式：GitHub Releases 发布 5 个目标平台的二进制，提供 `install.sh` / `install.ps1` 安装脚本和 cargo-binstall 元数据，`uniflo update` 按安装方式升级。不做 Homebrew。
- D15（用户，Q7=B）：恢复会话。
  - 命令输出：API、MCP、CLI 都给出结构化的恢复命令，并严格校验会话 id。
  - `uniflo resume <key>`：直接接续会话。
  - 打开终端：macOS 上 demo 可直接在 Terminal / iTerm2 / Ghostty 中打开；其他平台退回复制命令。
- D16（用户，Q9=a+b+c+f）：额外扩展四项：
  - agent 记忆与指令文件目录；
  - `uniflo context` 接力上下文，可选安装 Claude SessionStart hook；
  - 网页回合结束通知；
  - 活跃度洞察。
- D17（用户，Q10=A）："自动更新同步"只指价格和模型目录：每 6 小时经系统 curl 同步，主源 models.dev，LiteLLM 补位。Uniflo 自身升级仍需手动。
- D18（用户，Q4 自述）："额度"不接订阅额度接口，也不接外部额度提供方。
  - 数据来源：上游模型目录（价格、上下文窗口、最大输出）加本机监控会话的用量统计。
  - 统计粒度：每一步，即每次模型调用。
  - 每步字段：输入、输出、缓存读、缓存写、思考 token、费用、上下文占用。
  - 订阅用户的费用显示为"API 等价成本"。
- D19（用户）：作为 cargo 包，对外接口仍是核心。crates.io 仍是主渠道，预编译二进制是附加渠道。新能力进库 crate 和对外契约，demo 只是参考客户端。
- D20（规划）：拆成 Supervisor Change，共 7 个子任务，依赖关系见 `children.yaml`。推进方式待用户在最终确认时选择。

# 待解决问题

无。全部决定已在 D1–D20 中确认；推进方式在最终 Shape 确认时选择。

# 验证预期

- 每个子任务交付前跑 `scripts/verify.sh`：fmt、clippy `-D warnings`、各适配器 feature 单独编译、全量测试。动了网关或网页的加 `--e2e`。最终集成结果跑 `scripts/verify.sh --e2e`。
- 本机真实数据跑 `uniflo scan --no-cache --json`：`stats.unknown` 为空，`bad_lines` 与 `read_errors` 为 0。只输出计数、键名和金额合计。
- 费用：用手写的合成 usage 对照价格表手算校验。价格同步通过本地模拟源测试（URL 可由环境变量覆盖），不依赖外网。
- MCP / setup：
  - 在隔离 HOME 下用真实 harness CLI 跑通"setup → 列出工具 → 调用工具 → uninstall"。
  - 对用户的真实配置只做 `--dry-run`。真实写入需用户另行授权。
- 网页：真实浏览器加合成数据，覆盖明暗主题、宽屏与窄屏截图，对照参考设计逐项核对。
- 未执行或只能在 CI 执行的检查标"未验证"，并写明原因，例如：跨平台交叉编译产物、Linux / Windows 终端行为、无本机数据的 harness 真机格式。
