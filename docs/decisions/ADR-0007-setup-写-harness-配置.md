# ADR-0007 · setup 写 harness 配置

> `uniflo setup` 是"只读 harness 数据"的两个例外之一（另一个是 ADR-0006 的会话清理）：经用户确认后，只增删 harness 配置里名为 `uniflo` 的 MCP 条目、Skill 软链和一条可选 hook；Uniflo 直接编辑的文件先备份、原子写入，按记录对称撤销；会话文件和数据库仍然只读。

状态：`accepted` · 更新：2026-10-09

## 背景

让 agent 用上 Uniflo，要在每个 harness 的配置里注册 MCP 服务器或放一份 Skill。本机常见的 harness 有十几个，配置位置、格式（JSON / TOML、键名、条目形状）各不相同，手工逐个配置门槛太高，用户需要一条命令完成。

但 `AGENTS.md` 的契约是"任何适配器、测试、脚本都不得写入 harness 的会话文件或数据库"。harness 的配置文件不是会话数据，却同样属于用户：`~/.claude.json` 还是 Claude Code 运行中持续改写的状态文件，写坏或与它并发写都会丢用户的配置。

## 决策

允许 `uniflo setup`（只有它）写 harness 配置，边界如下（实现 `crates/uniflo-cli/src/setup/`，用法 `docs/agents.md#uniflo-setup`）：

1. **只写三类东西**：配置里名为 `uniflo` 的 MCP 条目；skills 目录里名为 `uniflo`、指向 `~/.agents/skills/uniflo` 的相对软链（以及这份 Skill 本身）；`~/.claude/settings.json` 的 `hooks.SessionStart` 里运行 `uniflo context` 的一组（仅 `--hook` 或交互里选"是"）。会话文件、数据库、其他条目一律不碰。
2. **优先用 harness 自己的命令**：claude、codex、gemini、droid、codebuddy、qodercli 用各自的 `mcp add/remove`，格式和并发写交给 harness 自己；只有没有命令的 harness 才直接编辑 JSON（`serde_json` `preserve_order` 保留键序），codex 不在 PATH 时用 `toml_edit` 保留格式编辑 `config.toml`。
3. **不覆盖别人的东西**：名字已被非 Uniflo 条目占用、软链位置已有别的文件，只提示不改；文件解析失败就跳过该 harness，不修复。
4. **可回退**：内容真的变化时才留 `.bak-uniflo-<时间戳>` 备份，写临时文件后 `rename` 原子替换；改了什么记在 `<配置目录>/setup.json`，`--uninstall` 先确认条目仍是 Uniflo 的再按记录删除，备份保留。
5. **必须经过确认**：终端里展示计划后确认；非终端只打印计划，只有 `--yes` 才执行；`--dry-run` 永不写。守护进程、`uniflo mcp`、CI、`UNIFLO_NO_SETUP=1` 从不提问也不写。自动询问只在首次交互运行和 `uniflo update` 之后各一次，拒绝会被记住。
6. **测试隔离**：设了 `UNIFLO_HOME` 时 setup 只在其下读写，调用 harness 命令时子进程的 `HOME` 改指向它并去掉 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`。测试与开发只在临时 HOME 里执行写操作，真实家目录只跑 `--dry-run`。

## 否决项

- **只打印配置片段让用户自己贴**：十几个 harness、几种格式，用户难以做对，也无法撤销；保留为非终端下的默认行为（只打印计划）。
- **所有 harness 都直接编辑配置文件**：`~/.claude.json` 等文件被运行中的 harness 并发改写，直接写有覆盖用户改动的风险；格式细节（作用域、条目形状）也随 harness 版本变化。有官方命令时交给命令。
- **把 SKILL.md 复制到每个 harness 的 skills 目录**：升级后各份内容不同步，读 `~/.agents/skills` 的 harness 还会加载两次；改为一个真源加相对软链，只给不读共享目录的 harness 建。
- **同名条目直接覆盖**：会破坏用户自己叫 `uniflo` 的配置；改为只提示。
- **引入 `rmcp` 等 MCP 框架**：服务器只需要 stdio 上的 `initialize` / `tools/list` / `tools/call`，协议层手写不到 100 行（`crates/uniflo-cli/src/mcp.rs`），不值得新增依赖。

## 后果

- 收益：一条命令接入本机所有检测到的 harness，可重复执行（幂等）、可撤销、换安装位置后 `--reload` 一次刷新。
- 代价：要跟踪各 harness 的配置位置与命令（`crates/uniflo-cli/src/setup/harness.rs` 的 `SPECS`）；harness 改格式时 setup 会报"解析失败"或命令失败，需要更新表。
- 已知差异：有官方 `mcp add/remove` 命令的 harness 由它自己的 CLI 改写配置，Uniflo 不另做备份。`codex mcp add/remove` 会省略其他条目里的 `args = []`、把行内 `env = {…}` 展开成子表，撤销后 `config.toml` 与原文件语义相同、字节可能不同。
- 新依赖：`toml_edit`；`serde_json` 全工作区开启 `preserve_order`（`serde_json::Value` 里对象的键按读入顺序输出，不再按字母排序；wire 上键序本来就不是契约）。
- `serde_json` 全工作区开启 `float_roundtrip`（已有依赖的特性，`Cargo.lock` 无变化）。默认的浮点解析不保证逐位还原，MCP 和 CLI 把守护进程的响应解析后再输出时，`cost_usd` 会在最后一位与 REST 不同（本机真实数据上相对差约 2e-16）。开启后解析是精确的，再用最短往返表示输出，与守护进程原样相同，所以 REST、CLI `--json`、MCP `structuredContent` 的数值逐位一致。代价只是解析浮点稍慢；`uniflo usage --json` 原样转发响应字节的做法保留不变。
- 同步：`AGENTS.md` 契约里写明这条例外；`docs/agents.md` 记录每个 harness 的注册方式。

## 复议触发

- 用户报告 setup 改坏或丢失了配置。
- 某个 harness 提供了插件 / 扩展安装机制，或改了 MCP 配置格式、命令参数。
- 需要写 harness 配置以外的东西（例如会话数据、凭据），必须另立决策。
