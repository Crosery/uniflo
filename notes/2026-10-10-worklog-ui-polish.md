# 2026-10-10 worklog · ui-polish

> service-expansion 子任务 ui-polish（A7）：用量视图不再使用原生控件，筛选与日期范围改用组件库，管理表勾选框统一组件样式。

状态：`historical` · 更新：2026-10-10

## 完成条件

- 用量视图没有可见的原生 `<select>` / `<input type=date>`；harness、模型、项目是与侧栏同一封装的 Tom Select，支持输入搜索、键盘选择、固定最大高度滚动、「全部 …」重置行。
- 选项带当前时间范围的会话数与费用，按费用降序；harness 带品牌图标；项目每个 key 一项，主行项目名、次行 `~/…` 路径，搜索匹配名与路径。
- 自定义日期是一个起止范围控件（Air Datepicker 3.6.0，内嵌），选满两个日期即写 URL 并刷新数据。
- 管理表勾选框用统一组件样式，全选在部分选中时为半选。
- 不新增除日期组件外的库；无运行时联网。

## 改动

- `examples/web/index.html` 与 `crates/uniflo-gateway/src/index.html`（字节一致）：
  - 共用封装 `makeCombo` / `syncCombo` / `positionCombo`（原侧栏 harness 下拉的逻辑抽出，CSS 类 `harness-combo` 改名 `combo`），侧栏与 `#u-h` `#u-m` `#u-p` 共用；用量加载多取一份 `group_by=harness`（不带其他过滤）作选项来源。
  - Air Datepicker JS/CSS 原样内嵌（`data-library="air-datepicker"`），主题变量、中文区域设置在页面自己的样式与脚本里；`#u-from` / `#u-to` 合并为只读的 `#u-dates`；点「自定义」自动弹出。
  - 勾选框 `input.cb`（`appearance: none`，强调色填充、对勾 / 横杠、焦点环，明暗主题）；`renderSel` 同步全选框的 checked / indeterminate。
- `THIRD_PARTY_NOTICES.md`：新增「网页演示内嵌的交互组件」一节，记 Tom Select 与 Air Datepicker 的版本、许可、来源与完整性值。Air Datepicker tarball 从 npm registry 用 curl 拉到空临时目录，`sha512` 与 registry `dist.integrity` 一致后只取 dist 文件。
- `scripts/demo-e2e.mjs`：合成数据加第二个名为 `web` 的项目（`/w/beta/web`，3 个会话）；新增步骤 `runUsageControls`；`runManage` 加勾选框断言。

## 原生控件审计

整页（排除内嵌库）含 `<select>` / `<input>` / `<textarea>` / `<datalist>`：

| 控件 | 处理 |
|---|---|
| `#harness-select`、`#u-h`、`#u-m`、`#u-p`（select） | Tom Select |
| `#u-from`、`#u-to`（date） | 合并为 Air Datepicker 范围控件 |
| `data-pick`、`#m-pickall`（checkbox） | 组件样式 `cb` |
| `#q`、`#s-q`（search）、`#s-f`、`#m-q`（文本） | 文本输入，保留（统一 `.input` 样式） |
| number / range / file / color / radio / time、`confirm` / `alert` / `prompt`、`<dialog>` | 页面中不存在 |

## Scenario 与证据

| 要求 | 证据 | 结果 |
|---|---|---|
| 无可见原生 select / date | `[usage-ui] filters are Tom Select combos…` | PASS |
| harness 选项图标 / 会话 / 费用 / 费用降序，与 API 逐项一致 | `[usage-ui] harness options…` | PASS |
| 输入缩小选项；选中后 KPI 与表等于 `/v1/usage` | `typing narrows…`、`harness = claude`、`choosing a harness…` | PASS |
| 模型过滤与重置 | `model options…`、`claude + opus`、`the 全部 rows reset…` | PASS |
| 同名项目分别出现且靠路径区分，搜索匹配路径，选中后 KPI 为 3 个会话 | `same-named projects…`、`project search matches the path`、`choosing a project…` | PASS |
| 键盘选择 | `keyboard navigation picks an option` | PASS |
| 日期范围：URL、查询、输入框一致，选满自动关闭 | `date picker applies…`、`custom <from>..<to>` | PASS |
| 勾选框可切换、全选半选 | `[manage] checkboxes use the component style…` 及原有管理流程 | PASS |
| 现有 Scenario 不回退（含可见文字对比度、溢出、重叠审计） | 整个 e2e 125 项 | PASS |

## 验证

- `CHROME=/Users/crosery/.local/bin/chrome-headless-shell scripts/verify.sh --e2e`：`{"ok":true,"passed":125,"failed":0}`，`verify: all checks passed`（含 `third-party-notices.mjs --check`）。第一次运行 `uniflo-cli` 的 `mcp_cli_and_rest_agree` 因 "timed out waiting for health" 失败，与本改动无关（网页与 e2e 之外无改动），原样重跑通过。
- 截图（逐张人工看过，下拉与日历均未被裁切）：`notes/evidence/2026-10-10-web-console/usage-{1480,390}-{dark,light}-{project,date}.png`，分别是项目下拉展开、日期组件展开。
- 未验证：真实数据页面（只用合成数据，没有碰 7311 的守护进程）。
