# Git 与分支规范

> 分支怎么拉、改动怎么进主干、哪些 Git 操作需要授权。

状态：`current` · 更新：2026-10-04 · 依据：参考 `/Users/crosery/work_file/geek_main/docs/conventions/BRANCHING.md`

## 分支模型

- 采用 **`main`（正式稳定）与 `stage`（预发布集成分支）** 双长期分支模型，详见 [`docs/conventions/BRANCHING.md`](BRANCHING.md)。
- 规则：`stage` ≥ `main`，所有新功能与需求先在 `stage` 开发与验证；稳定发版时由 `stage` 合入 `main`。
- 任务分支：`task/<issue>/<slug>` 或 `task/<slug>`，从 `stage` 拉出，分支名一律不含 `-`，只用 `/` 分层，段内用 `_` 连接，合并回 `stage` 后立即删除。
- 动手前先 `git branch --show-current` 和 `git status`，确认在正确分支（如 `stage`）、没有别人的未提交改动。
- 门禁脚本：`node scripts/check-branch-invariants.mjs`。

## 进主干

- 改动经 MR/PR 合入；描述写清为什么、改了什么、怎么验证，并附审查结论。
- 审查用 `code-review` 技能逐项核对 diff；结论只写「阻塞 / 有条件通过 / 通过」三选一。

## 需要授权的操作

推送、合并、打 tag、删除远端分支、`reset --hard`、`clean -fd`、`push --force`、改写已推送历史：每次都要用户明确同意。脏工作区里先报告，再由用户决定。

## comet 流程

`/comet` 归档时只选「仅归档并保留工作区（keep）」；merge、push、PR 按本页走，不用 comet 代劳。
