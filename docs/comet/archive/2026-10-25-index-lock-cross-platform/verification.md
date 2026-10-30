---
generated_from_state_version: 13
---

# 验证

## 当前结果

- 结果: **已归档**
- 验证情况: **已完成检查，验证结果已确认**
- 目标周期: 1
- 迭代: 3
- 验证器尝试次数: 1
- 完成时间: 2026-10-25T19:32:09.449Z
- 摘要: 0.2.3 候选全部 8 项验收范围（A1–A8）经独立核查判定全部通过。全文索引写租约与跟随者接管、SQLite 暂态争用与忙库安全、Windows 占用与精确回滚、Linux NUL 参数、macOS 多根别名通知、工作队列按进入顺序固定与同批重入、统一门禁（131.443s 及 6126 会话真实扫描）以及正式 Release（7 个 crate、5 个发布包及 SHA256SUMS、GitHub Release 8 项资产、官网和真实程序 smoke）均已实测通过并核对一致。

## 验收

| 编号 | 结果 | 来源 | 验收项 | 原因 |
| --- | --- | --- | --- | --- |
| A1 | passed | brief.md | 同一缓存目录有守护进程和 MCP/第二索引进程时，查询仍可用，不出现持续竞争写入；已有命中保留，新内容在写者中继续可检索。 | 实现基于 .writer SQLite 独占写租约保证单写者，跟随者基于 data_version pragma 刷新只读连接并同步内存映射与进度；写者退出时跟随者自动竞争接管；未提交命令计数器直到提交成功才扣减，回滚时 requeue_commands 完整重回队首；MCP 一次性建库后主动调用 release_writer_when_idle 释放租约，重新探测到守护进程后自动清空 local 缓存并切回 daemon。共享多读与接管单测、一次性租约释放单测均通过。 |
| A2 | passed | brief.md | 索引遇到暂态 SQLite 写竞争时，不丢弃已接收的索引内容，也不把失败的 pass 标为已完成。 | 写事务改为 BEGIN IMMEDIATE，杜绝 WAL 延迟事务升级冲突（错误码 517）；写竞争失败触发 rollback、置位 lagged 并将 pass 置为 None，不标记完成并重试全部未提交请求；store::open 仅对确认的数据库损坏删除重建，遇到锁与 I/O 错误保留磁盘索引。修改后的真实锁争用测试 pinned_content_is_retried_after_a_busy_transaction 给予 20s 异步调度余量，完整保留外部并发锁、报错计数增加、未提交前保持非 idle 及解锁后精确检索到 pinned 事件的全部断言。 |
| A3 | passed | brief.md | Windows 经确认的缺陷有修前失败、修后成功证据，并在 Windows 真实程序或 CI 上验证。 | move_aside 在目标 .old 被占用时回退至独立三段式备份名 <exe>.old-<now:x>-<pid:x>-<seq:x>；最终 staged 覆盖失败时严格按本次备份执行回滚；remove_aside 与 install.ps1 严格匹配三段式十六进制正则，保留 Unicode 文件及 .old-dead/.old.bak 等非自身产物。LuoXi 真机 0.2.2 基线报 OS error 5 exit 101，0.2.3 同程序 exit 0；swap_aside_windows_file_locking 单测通过；PowerShell 5.1 完整安装与备份保留通过；CI 40224069422 Windows runner 5m49s 通过；发布后实际 x86_64 二进制在 LuoXi 真机实跑通过。 |
| A4 | passed | brief.md | Linux 经确认的缺陷有修前失败、修后成功证据，并在 Linux 真实程序或 CI 上验证。 | Linux 读取 /proc/<pid>/cmdline 的 NUL 字节切分得到完整 p.argv；arg_after 优先按数组边界提取参数值，带空格参数不被截断。arch-linux 真机 0.2.2 基线截断为 /tmp/work exit 101，0.2.3 完整提取 exit 0；linux_discovers_child_with_spaced_resume_arg 单测通过；CI 40224069422 Ubuntu runner 2m53s 通过；发布后实际 x86_64 发布包在 arch-linux 运行 daemon 验证 version 0.2.3、health=true。 |
| A5 | passed | brief.md | macOS 经确认的缺陷有修前失败、修后成功证据，并在本机真实程序上验证。 | map_canonical_path 遍历所有配置根，将 FSEvents 规范化路径双向映射回所有匹配的前缀（同时通知软链接别名与物理根）。真实 FSEvents 探针修前漏通知物理根 exit 101，修后双根均通知 exit 0；watcher 集成单测通过；CI 40224069422 macOS runner 4m10s 通过；发布后 arm64 与 x86_64 发布包实际运行 daemon-smoke（health 0.2.3、33 harness、合成 session、demo、no-fts 503）全部通过。 |
| A6 | passed | brief.md | 工作中会话、子代理按本轮 work 进入顺序排列；更新、加载更多、刷新不改变同一工作轮次顺序，idle 后再次 work 排到队列末尾，空闲列表仍按最近活动排列。 | Entry::refresh 识别同批读取中 TurnEnd 后紧随新输入且无 live override 的情形为 restarted，推进 status_since 至新工作时间戳；sig 包含 status_since，驱动 SSE 广播快照；Web 端以 workSince 排序，家族槽位记录在 sessionStorage 并在子代理成员交替结束时保持连续；全家 idle 后重新 work 排到队尾；isPureWorkFilter 确保 s:work 筛选下活动更新不发生跳位。4 项 core 单测、work-turn-batch-final（status_since 匹配新 user 1792955088196）以及网页端 E2E_ONLY=runTree 19 项交互测试全部通过。 |
| A7 | passed | brief.md | 统一门禁与网页 e2e 全部通过，worklog 区分已验证与未验证，既有搜索、会话与用量路径不回退。 | 当前候选绑定的 4 项 Runtime 检查全部通过：verify-e2e（131.443s，全量 Rust 测试、Clippy、167 项网页 e2e）、work-turn-batch-final（771ms）、real-scan（7779ms，6126 会话零未知/坏行/读错）、site-build（130ms，10页 v0.2.3）；三平台 CI 40224069422 全绿通过；worklog 区分已验证与未验证边界，默认忽略项 real_claude_and_codex_setup_in_an_isolated_home 明确记录为未运行；既有搜索、会话和用量接口零回退。 |
| A8 | passed | brief.md | 用户明确确认新版本后发布补丁版；GitHub Release 的 5 个包与校验和、7 个 crate、官网和仓库版本与发布说明一致。 | 用户明确授权后完成发布；GitHub Release v0.2.3 包含 8 项完整资产（5 个平台发布包、SHA256SUMS、install.sh、install.ps1），isDraft=false，isPrerelease=false，publishedAt=2026-10-25T19:21:14Z；shasum 校验 5 包全部 OK，脚本与验收源码一致；官方稀疏索引 7 个 crate（uniflo-schema、uniflo-core、uniflo-adapters、uniflo-search、uniflo-gateway、uniflo-client、uniflo）全部为 0.2.3 且 yanked=false；官网与 console.html 均上线 v0.2.3 且包含本轮工作顺序规则；真实发布包在 macOS（arm64、x86_64）、Linux（x86_64）、Windows（x86_64）上实际运行 smoke 全部通过。 |

## 检查

| 检查 | 命令 | 工作目录 | 状态 | 退出码 | 耗时 |
| --- | --- | --- | --- | ---: | ---: |
| Full Rust and web gate with CI toolchain | RUSTUP_TOOLCHAIN=1.99.0 CARGO_TARGET_DIR=target/clippy-199 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 scripts/verify.sh --e2e | . | passed | 0 | 131443 ms |
| Actual isolated daemon batched-turn status boundary after final build | /tmp/uniflo-lock-probe.xWyUHR/turn_batch.py | . | passed | 0 | 771 ms |
| Read-only real harness scan, counters only, all storage isolated | /tmp/uniflo-lock-probe.xWyUHR/scan_counts.py | . | passed | 0 | 7779 ms |
| Build v0.2.3 website from repository docs | site/build.mjs target/site | . | passed | 0 | 130 ms |

### Builder 报告的证据

以下为 Builder 报告，不等同于 Runtime 检查凭据或独立验收结果。

- Targeted Rust behavioral regressions: passed — core lib 97, watcher integration 1, search 69, MCP recovery 1; clippy default toolchain passed; final helper targeted tests passed.
- Windows real-device red/green: passed — LuoXi baseline swap OS5 exit101; 0.2.3 same program green. Exact native lock+rollback test1 passed. PS5.1 full original installer exit1 versus fixed installer exit0 using loopback package server and temporary HOME/install/config/cache; held and unrelated backups preserved.
- Linux real-device red/green: passed — arch-linux real child spaced resume path baseline truncated exit101; 0.2.3 full argument exit0; exact kernel-child regression1 passed.
- macOS actual FSEvents red/green: passed — Before alias=true/canonical=false; after both=true, actual watcher and engine integration pass.
- Gateway browser working queue: passed — E2E_ONLY=runTree 19 passed/0 failed, including rework tail, child/family continuity, reload, filtered refresh and adjacent folding/navigation.
- Batched turn and stale work reentry: passed — Actual daemon red reproduced same-batch TurnEnd+UserMessage retaining the old work time. Both new unit regressions failed before repair; after repair engine refresh tests4 pass, covering live precedence, partial end, stale wake and transcript replay. SpecReview focused review passes. Actual fixed CLI smoke is included in the formal plan.
- CI contention synchronization repair: passed — First stage CI passed Windows/Linux but macOS timed out in the new8s contention wait. Same real SQLite lock test now uses the existing20s eventual budget, retaining all error/pending/content preservation assertions; exact targeted test1 passed in7.01s. No production code or timeout changed; final full gate and fresh three-platform CI must still pass.
- Packaging, notices and website build: passed — check-dist5 targets, notices194 crates/113 license texts up to date, site10 pages v0.2.3, branch invariants pass.
- 已知限制: One pre-existing ignored real_claude_and_codex_setup_in_an_isolated_home test requires actual third-party CLIs and is not counted as run; setup is unchanged.

## 阻塞项

_无。_

## 风险与跳过的工作

- Linux aarch64 预编译发布包已由原生 ARM CI 成功构建并通过 SHA256SUMS 校验，但本轮未在真实的 ARM Linux 机器上额外实机运行。
- 默认忽略测试 real_claude_and_codex_setup_in_an_isolated_home 未运行，原因与源码注释一致：'uses the real claude and codex CLIs'；本轮未改动该 setup 逻辑，未将该项计为已运行通过。

## 之前的迭代

| 目标周期 | 迭代 | 尝试 | 结果 | 未解决项 | 摘要 | 完成时间 |
| ---: | ---: | ---: | --- | --- | --- | --- |
| 1 | 1 | 0 | recovery | — | Builder handoff Runtime checks failed: verify-e2e, real-scan | 2026-10-25T18:36:49.283Z |
| 1 | 2 | 1 | recovery | — | macOS stage CI failed the contended-write regression before publication. Inspect and repair deterministic contention/error synchronization without skipping or rerunning away the failure; no requirement change. | 2026-10-25T18:57:57.839Z |
| 1 | 3 | 1 | pass | — | 0.2.3 候选全部 8 项验收范围（A1–A8）经独立核查判定全部通过。全文索引写租约与跟随者接管、SQLite 暂态争用与忙库安全、Windows 占用与精确回滚、Linux NUL 参数、macOS 多根别名通知、工作队列按进入顺序固定与同批重入、统一门禁（131.443s 及 6126 会话真实扫描）以及正式 Release（7 个 crate、5 个发布包及 SHA256SUMS、GitHub Release 8 项资产、官网和真实程序 smoke）均已实测通过并核对一致。 | 2026-10-25T19:32:09.449Z |



## 结论

0.2.3 候选全部 8 项验收范围（A1–A8）经独立核查判定全部通过。全文索引写租约与跟随者接管、SQLite 暂态争用与忙库安全、Windows 占用与精确回滚、Linux NUL 参数、macOS 多根别名通知、工作队列按进入顺序固定与同批重入、统一门禁（131.443s 及 6126 会话真实扫描）以及正式 Release（7 个 crate、5 个发布包及 SHA256SUMS、GitHub Release 8 项资产、官网和真实程序 smoke）均已实测通过并核对一致。
