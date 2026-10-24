# docs 地图

> Uniflo 的规范、架构与决策都在 `docs/`；本页是入口，按任务找文档。

状态：`current` · 更新：2026-10-25

| 目录 / 文件 | 内容 |
|---|---|
| `architecture.md` | 数据流、读取模型、状态机、存活探测、并发模型 |
| `schema.md` | wire schema v1：`Session` / `Event` / `Envelope`（对外契约） |
| `api.md` | 网关 REST / SSE / NDJSON / WebSocket 与安全规则 |
| `sdk.md` | Rust SDK 选型：`uniflo-client`（连接守护进程）、`uniflo-schema`（只要类型）、嵌入式使用（`uniflo-core`/`uniflo-adapters`/`uniflo-search`/`uniflo-gateway`） |
| `search.md` | 会话搜索语法（fd 式过滤 + fzf 式模糊）与全文检索语法 |
| `adapters.md` | 已支持的 harness 与新增适配步骤 |
| `agents.md` | agent 接入：MCP 服务器、Skill、`uniflo setup`、恢复会话、记忆与指令文件、接力上下文 |
| `console.md` | 网页控制台（`/demo`）：六个视图、命令面板与通知、布局与 URL 参数、依赖与验收 |
| `conventions/DEVELOPMENT.md` | 模块边界、代码风格、性能预算、隐私红线 |
| `assets/` | README 与 `console.md` 的截图，由 `scripts/demo-e2e.mjs` 用合成数据生成，不含真实会话；头图 `banner.jpg` 由 gpt-image-2.5 生成；开发者站构建时整个目录随站发布 |
| `conventions/COMMITS.md` | 提交信息与原子性 |
| `conventions/BRANCHING.md` | main / stage 双长期分支模型与硬不变量 |
| `conventions/GIT.md` | 分支、合并、推送与授权边界 |
| `conventions/DOCUMENTATION.md` | 文档放哪、怎么写、状态词表、同步规则 |
| `conventions/TESTING.md` | 验收标准、证据分级、完成条件 |
| `decisions/` | 决策记录（`ADR-NNNN-中文标题.md`，模板 `decisions/ADR-0000-模板.md`）；`accepted` 已接受，`proposed` 待定（如 ADR-0011 全文索引紧凑格式） |
| `comet/` | comet Native 流程产物（需求、规格、归档），由 `/comet` 维护，不手改 |

开发者站：`node site/build.mjs <out>` 把本目录和根 `README.md` 渲染成静态站并生成 `llms.txt`，推到 main 时由 `.github/workflows/pages.yml` 发布到 GitHub Pages；页面模板与样式在 `site/`。

过程记录与证据不放这里：`notes/YYYY-MM-DD-worklog.md` 记每天做了什么，`notes/evidence/` 放日志、截图等证据。
