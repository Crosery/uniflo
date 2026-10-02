# Git 与分支规范

> 分支怎么拉、改动怎么进主干、哪些 Git 操作需要授权。

状态：`current` · 更新：2026-10-02

## 分支模型

- 默认分支：`main`，无预发布分支；`main` 必须始终能通过 `scripts/verify.sh`。
- 任务分支：`task/<issue或日期>-<slug>`，从 `main` 拉出，合并后删除。
- 动手前先 `git branch --show-current` 和 `git status`，确认在正确分支、没有别人的未提交改动。

## 进主干

- 改动经 MR/PR 合入；描述写清为什么、改了什么、怎么验证，并附审查结论。
- 审查用 `code-review` 技能逐项核对 diff；结论只写「阻塞 / 有条件通过 / 通过」三选一。

## 需要授权的操作

推送、合并、打 tag、删除远端分支、`reset --hard`、`clean -fd`、`push --force`、改写已推送历史：每次都要用户明确同意。脏工作区里先报告，再由用户决定。

## comet 流程

`/comet` 归档时只选「仅归档并保留工作区（keep）」；merge、push、PR 按本页走，不用 comet 代劳。
