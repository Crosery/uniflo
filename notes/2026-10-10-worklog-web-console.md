# 2026-10-10 worklog · web-console

> service-expansion 子任务 web-console：网页演示扩成五个视图（会话 / 用量 / 检索 / 管理 / 洞察），harness 品牌图标，回合结束通知，全部状态进 URL；e2e 全部用合成数据、临时 `UNIFLO_HOME` 与 `UNIFLO_TRASH_DIR`，打开终端与系统通知都在页面内拦截。

状态：`historical` · 更新：2026-10-10

## 01:30 · 网页控制台

### 完成条件

- `docs/comet/changes/service-expansion/specs/web-console/spec.md` 的 8 个 Scenario 都有 `scripts/demo-e2e.mjs` 的自动断言，数值一律与对应 REST 接口逐项比对，不只看"有渲染"。
- 5 个视图 × 1480 / 390 px × 明暗主题共 20 张截图，自动检查横向溢出、工具栏 / KPI / 选择条内元素重叠、所有可见文字 WCAG AA 对比度；截图逐张人工看过。
- `examples/web/index.html` 与 `crates/uniflo-gateway/src/index.html` 字节一致，且守护进程 `/demo` 返回的就是它。
- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e` 通过。

### 改动与依据

- `d27ae86` gateway：`GET /v1/harnesses/{id}/icon.svg`（`crates/uniflo-gateway/src/icons.rs`，从内嵌页面的 `#harness-icons` 雪碧图切出，图标只存一份），`Harness.icon`（schema 只增，`docs/schema.md` 同提交）。lobe-icons 1.95.1 单色图标 22 个，另 11 个 harness（pi、omp、crosery、prime、kodu、zcode、workbuddy、factory、reasonix、dsh、craft）无上游图标，前端画字母块。许可证与映射写进 `THIRD_PARTY_NOTICES.md`，MIT 全文同时内嵌在页面注释里（cargo 包只带页面）。
- `38677d4` core：洞察的星期 × 小时分布需要 API 现成的分组，新增 `group_by=weekday_hour`（键 `3-09` = 周三 09:00–10:00）；用量视图"按模型堆叠"的逐模型日序列需要按模型过滤，新增 `model` 参数。两者经 gateway、CLI（`--model`、`--by weekday_hour`）、MCP 枚举同步，`crates/uniflo-gateway/tests/usage.rs` 断言 weekday_hour 按星期 / 按小时求和分别等于 weekday / hour 分组、model 过滤后的各分组合计等于 `group_by=model` 的对应行。
- 网页（本提交）：
  - 顶栏 `#nav` 五个视图，`view` 与各视图状态（`range` `from` `to` `uh` `um` `up` `group` `under` `sort` `asc` `stack` `unpriced` · `sq` `sf` `at` · `mtab` `mq` · `metric` `rank`）写进 URL，只写非默认值；`popstate` 恢复；数字键 1–5 切视图。
  - 会话头：API 等价成本、五类 token、最后一步上下文占用条；「每步用量」逐步表；「恢复」菜单复制 resume 命令，macOS 且非只读时可在 Terminal / iTerm2 / Ghostty 中打开（`POST …/open-terminal`，带 `X-Uniflo-Write: 1`）。
  - 用量：KPI（总费用旁标"API 等价成本"，未定价步数可展开到模型列表）、按 harness / 模型堆叠的每日费用柱图（悬停与方向键提示，图例带金额）、可排序明细表（目录分组可两级以上下钻，面包屑）、CSV 导出。
  - 检索：`/v1/search` 结果按会话分组、`mark` 高亮；点击片段用 `events?around=` 打开并滚动高亮该事件，窗口未到最新时给"跳到最新"。
  - 管理：列表的大小与能否清理来自一次只读的 dry-run 计划；勾选（支持 Shift 连选）→ 计划审阅（可释放空间、预计归档、不可清理原因）→ 确认执行 → 逐项结果；归档页删除需二次确认；`--read-only` 时隐藏全部写控件并说明。
  - 洞察：一年热力图（`group_by=day`）、星期 × 小时（`weekday_hour` + 两侧 `weekday` / `hour` 合计）、项目 / 模型 / harness 前 5（`sort=<指标>&limit=5`）。
  - 通知：铃铛按钮，默认关，`localStorage` 记住；页面不可见时会话 work → idle 才通知，标题为会话标题，同一会话 30 s 内一次，点击打开会话。
  - 视觉：文字色阶提到 AA（暗色 `--text-2/3/4` = .70/.60/.54，亮色 `#4a4a4a/#5e5e5e/#686868`，亮色 `--ok` `#2b7019`），分类色板用 dataviz 校验脚本在两种主题底色上通过，热力图用单色顺序色阶。
- e2e：`scripts/e2e-fixtures.mjs` 为全部 33 个 harness 手写合成会话（只写临时目录）；`scripts/demo-e2e.mjs` 新增 60 天多项目 / 多模型用量、检索目标、可清理与运行中会话、只读守护进程（第二个端口），页面里注入审计脚本；`E2E_ONLY=<步骤名,…>` 只跑部分步骤。第 18 行 `CHROME` 默认值未改。

### 失败与教训

- 审阅页一开始就报"生成计划失败：Cannot read properties of undefined"：`CleanupCandidate.children` / `targets` 为空时不序列化，页面按数组直接读。改成可选读取；以后读 wire 对象的可选数组一律按缺省处理。
- 管理页在 390 px 横向溢出 322 px：`.page` 是 grid，子项默认 `min-width: auto`，表格的 min-content 把整列撑宽（`.table-wrap` 的横向滚动不起作用）。给 `.page` 设 `grid-template-columns: minmax(0, 1fr)` 并让子项 `min-width: 0`；手机上管理表隐藏状态 / 目录 / 最后活动列。
- 亮色 `--ok` 在侧栏和 tag 底色上只有 4.19–4.48:1，加深到 `#2b7019`（≥ 5.2:1）。
- 执行结果断言第一次误报：`waitFor` 把空数组当成"已就绪"立即返回，改为长度非零才算完成。
- 人工看图后的调整：未定价步数并进总费用卡片（原先单独一张卡掉到第二行）；用量状态行并入筛选栏（空行占 36 px）；热力图格子随面板放宽、右侧留出月份标签；星期 × 小时在窄屏改单字星期标签与竖长格子，24 小时全部放下并加图例；手机会话头只留费用与上下文条，token 明细在「每步用量」里。

### Scenario 证据

| Scenario | 自动断言（`demo-e2e.mjs`） | 截图 |
|---|---|---|
| 1 harness 图标 | `/v1/harnesses` 22 个带 `icon`、11 个不带；`icon.svg` 为 `image/svg+xml`；33 个 harness 的行逐一是 `<use href="#hi-<id>">` 或两字母块；明暗主题下图标对行背景 ≥ 3:1 | `icons-dark.png`、`icons-light.png` |
| 2 用量 | 7 天 → 按模型：KPI 与表格每个数值等于 `/v1/usage`（页面把实际请求写在 `#v-usage[data-query]`）；未定价展开为 `demo-model`；"API 等价成本"标注；目录 `/w/alpha` → `/w/alpha/api` 两级下钻面包屑；费用升序；刷新后视图、区间、分组、下钻、排序、表格行全部恢复并再次逐项等于接口 | `usage.png`、`visual-usage-*` |
| 3 会话详情 | 逐步表 3 行的 event / 五类 token / 费用 / 上下文百分比等于 `/v1/sessions/{key}/usage`；上下文条 = 最后一步 `context_pct`（5.09%）；剪贴板内容等于 resume 接口的 `command`；macOS 菜单有"在 Terminal 中打开"，点击发出带 `X-Uniflo-Write: 1` 的 `POST …/open-terminal?terminal=terminal`（页面内拦截，没有打开任何窗口） | `detail.png` |
| 4 检索 | 搜唯一词"量子退火调度器"：首个命中属于 `claude:search-target`，事件 id 等于 `/v1/search` 返回，`mark` 文本正确；点击后会话被选中、目标事件在可视区内且上下文前后都有事件、URL 带 `at=` | `search-jump.png` |
| 5 管理 | 勾选可清理 + 运行中两项 → 审阅：可释放 = 列表里的大小（11207 B），不可清理原因"会话运行中"；执行结果 `claude:cleanme=archived`，源文件进了临时 `UNIFLO_TRASH_DIR`；会话列表与管理表都显示"已归档"，归档页列出；二次确认删除后 `/v1/archive` 不再含它；只读守护进程：无复选框、无生成计划 / 全选 / 删除 / 清理 / 打开终端，显示 `--read-only` 说明 | `manage-review.png`、`manage-result.png`、`manage-archive.png`、`readonly-manage.png` |
| 6 洞察 | 会话与 token 两种指标下：热力图每格等于 `group_by=day`（49 个活跃日）；51 个星期 × 小时格等于 `weekday_hour`，两侧合计等于 `weekday` / `hour`；按费用与 token 的前 5 排行等于 `group_by=project|model|harness&sort=…&limit=5` | `insights.png`、`visual-insights-*` |
| 7 通知 | 替换 `Notification`、`visibilityState` 设为 hidden：默认关时无通知；打开后同一会话 30 s 内两次回合结束只通知一次，另一会话再通知一次，标题分别是两个会话标题，带 harness 图标与回合用时；点击通知打开该会话；关掉后不再通知 | — |
| 8 视觉 | 20 张截图齐全；无横向溢出、无重叠、全部可见文字达到 AA；两份页面字节一致且 `/demo` 返回同一份 | `visual-<view>-<1480|390>-<dark|light>.png` |

证据目录 `notes/evidence/2026-10-10-web-console/`：上述截图（按 CSS 像素宽度缩放、oxipng 无损压缩）和本次全量 e2e 输出 `e2e-run.txt`（端口已替换）。

### 验证

- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e` → `verify: all checks passed`：rustfmt、clippy `-D warnings`、24 个适配器 feature 单独编译、branch invariants、`cargo test --workspace` 23 个测试二进制 280 passed / 0 failed / 1 ignored、release 构建 + daemon smoke、e2e `{"ok":true,"passed":86,"failed":0}`。之后只改了 `demo-e2e.mjs` 头注释（让 `CHROME` 默认值留在原第 18 行），单独重跑 `CHROME=… bun scripts/demo-e2e.mjs` 仍为 86 / 0。
- 未改适配器，未对真实数据运行任何命令；e2e 守护进程只用临时 `UNIFLO_HOME` / `UNIFLO_TRASH_DIR`，并清掉 `CLAUDE_CONFIG_DIR`、`CODEX_HOME`、`XDG_DATA_HOME`、`UNIFLO_*_DIR` 等会把适配器指回真实目录的环境变量；端口随机（未碰 7311）。

### 未验证

- 真实终端窗口的打开（按约束在页面内拦截，只验证请求）与真实系统通知权限弹窗（替换了 `Notification`）。
- 只在 chrome-headless-shell 上跑过；Safari、Firefox 未测。
- `THIRD_PARTY_NOTICES.md` 是本子任务新建，distribution 子任务可能并行修改同一文件，合并时需按小节合并。
