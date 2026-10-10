# 2026-10-10 worklog · acceptance-fixes

> service-expansion 子任务 acceptance-fixes：用户手测集成版发现的三个缺陷（A5、A7）：打开终端从不开窗、11 个 harness 没有品牌图标、检索视图看不懂

状态：`historical` · 更新：2026-10-10

## 完成条件

- 打开终端不使用 Apple events：Terminal / iTerm 走 `open -a <app> <随机私有目录>/resume.command`（目录与文件 0700，脚本先删自己），Ghostty 走 `open -na Ghostty --args --working-directory=… -e /bin/sh -c <命令>`；失败返回中文 `reason`（同 `error`）并附 `command`；网页失败时留一个可操作的提示，带「复制恢复命令」。
- 11 个无图标 harness 里能找到官方标识的都补上，其余在文档里写明原因；每个来源与许可记入 `THIRD_PARTY_NOTICES.md`。
- 检索视图有一句话说明、两个可点的示例、按 `/v1/stats` 的 `fts` 区分未启用 / 建立中（进度与 ETA）/ 就绪。
- 任何测试与检查都不打开真实终端窗口（用户自己做窗口手测）。

## 改动与依据

- `d66ffcd` `resume::terminal_script` / `terminal_argv` / `ghostty_argv` 替换 `osascript` argv；`agent.rs` 里 `write_script` 用 `tempfile::Builder`（随机名，`permissions(0o700)`）建目录、`create_new` 以 0700 写脚本；`Launcher` 签名不变，`Env` 增加 `tmp` 以便测试指向临时目录。命令文本只出现在 `shell_line` 的 POSIX 转义里，没有任何解释器源码拼接。`tempfile` 由 dev 依赖升为网关运行时依赖（已在工作区依赖树内），因此重新生成了 `THIRD_PARTY_NOTICES.md`（`300403f`）。
- `518fa24` 新增 8 个图标：`pi`、`dsh`（DeepSeek）、`zcode`（Z.ai）取 lobe-icons；`omp`、`reasonix`、`craft`、`factory`、`workbuddy` 取各家官方标识并转成单色、用 `transform` 缩放到 24×24（网关独立 SVG 固定 `viewBox 0 0 24 24`，所以不能靠 symbol 自己的 viewBox）。仍是字母块：`crosery`（只有位图）、`prime`（官方站点本次连接超时，仓库无矢量标识）、`kodu`（AGPL-3.0 仓库且无单独标识授权，域名已出售）。来源、许可见 `THIRD_PARTY_NOTICES.md`「harness 官方标识」一节。
- `897084e` 检索视图：说明、示例 chip、`#s-index` 状态区；`/v1/stats` 的 `fts` 为 `null` 时只能说「`--no-fts` 或索引无法打开（见日志）」，因为守护进程不记录打开失败的原因，状态里没有可显示的原因，不另改后端；`errors>0` 时显示 `last_error`。建立中每 2 秒刷新，ETA 取最近 90 秒内的完成速率，两次采样前显示「估算中」，建好后自动重跑当前查询。

## 验证

| 命令 | 结果 |
|---|---|
| `cargo test -p uniflo-core -p uniflo-gateway` | 通过；含 `launches_open_with_a_private_script_or_ghostty_argv`、`failures_carry_a_reason_and_the_command_and_leave_no_script`、`terminal_launch_needs_no_apple_events`、集成测试 `launches_through_open_without_apple_events` |
| `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e` | `verify: all checks passed`，e2e `{"ok":true,"passed":107,"failed":0}`；输出见 `notes/evidence/2026-10-10-web-console/e2e-run.txt` |

| Scenario / 缺陷 | 证据 | 结论 |
|---|---|---|
| macOS 打开终端（argv、脚本文本、cwd 含 `'` 空格 `$`、0700、失败带 reason + command、403 / 422 / 501） | 上述单元与集成测试；网页失败提示：e2e `[detail] open-terminal failure keeps a toast…`（页面内桩，不开窗口） | PASS |
| 真实打开窗口 | 按约定不测，留给用户手测 | 未验证 |
| harness 图标 | e2e `[icons] pi, omp, workbuddy, factory, reasonix, dsh, zcode, craft draw brand icons; only crosery, prime, kodu keep the letter block`：30 个带图标、3 个字母块；明暗主题对比度 ≥ 3:1；`icons-dark.png`、`icons-light.png` 人工看过 | PASS |
| 检索视图说明、示例、三种索引状态 | e2e 的 `[search-help]` 8 项：说明文字、就绪行、两个示例、点击示例触发搜索、建立中（进度与「估算中」→ ETA → 就绪，桩 `/v1/stats`）、`--no-fts` 守护进程（专用端口，只读，合成数据）显示未启用并禁用表单 | PASS |

截图（`notes/evidence/2026-10-10-web-console/`）：`icons-dark.png`、`icons-light.png`、`detail.png`、`search-jump.png`、`search-help.png`、`search-building.png`、`search-off.png`，以及 20 张 `visual-<view>-<1480|390>-<dark|light>.png`（含检索与会话列表）。

## 追加：检索视图空状态

未输入或无结果时显示 `#s-home`：「正在运行」（无则「最近会话」，来自页面已有的会话数据，3 秒刷新，侧栏有过滤时另取 `/v1/sessions?limit=60`）、全文索引卡（已索引会话 / 大小 / 上次建立耗时；未启用时给原因和开启方法）、「试试这些」6 个示例（带过滤的会同时填过滤框）、检索语法速查、浏览器本地的「最近搜索」（localStorage，可清除）。宽屏两栏、窄屏单栏。e2e `[search-home]` 10 项：运行 / 最近行、索引卡与语法、4 种宽度×主题的溢出 / 重叠 / 对比度审计、点击行打开会话、点击示例同时填 query 与 filter 并搜索、最近搜索出现与清除；截图 `search-home-{1480,390}-{dark,light}.png`，已人工看过。索引状态里没有「最后更新时间」字段，所以不显示。

## 未验证 / 残余风险

- 没有开真实 Terminal / iTerm / Ghostty 窗口。Ghostty 首次用 `-e` 会弹它自带的确认框，且有已知的「命令执行两次」上游报告，需手测。
- 「建立中」「就绪」的真实守护进程路径只在桩数据下验证（合成数据索引瞬间完成）。
- `workbuddy`、`factory` 的标识取自官网，没有单独许可文件。
