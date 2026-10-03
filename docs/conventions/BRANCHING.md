# 分支模型规范

> 只有 `main`（正式稳定版）与 `stage`（预发布与动态集成分支）两条长期分支；task 分支合并后必须立即删除，任何操作前先确认当前分支。
> 规范参考：`/Users/crosery/work_file/geek_main/docs/conventions/BRANCHING.md`。

状态：`current` · 更新：2026-10-04

## 第负一步：先确认分支

**任何提交、推送、切分支、改文件之前，先跑 `git branch --show-current` 确认自己在哪条分支上。**
不在预期分支上时停止操作并说明，不能靠 `git checkout .` 或 `reset --hard` 来盲目纠正。

## 长期分支

| 分支 | 角色 | 生命周期 | 发布与流向 |
|---|---|---|---|
| `main` | 正式稳定版，只能由 `stage` 合入（发版时快进到被验收的提交） | 长期 | `vX.Y.Z` 稳定发布 tag |
| `stage` | 动态预发布与集成分支，所有新功能与需求先上预发布 | 长期 | `vX.Y.Z-rc.N` 预发布验证 |

除这两条以外，**不允许存在第三条长期分支**。

## 短生命周期分支

| 分支 | 来源与去向 | 规则 |
|---|---|---|
| `task/<issue>/<slug>` 或 `task/<slug>` | 从 `stage` 拉出 → 验证后合回 `stage` | **极短**：合并后必须立即删除，禁止残留死分支。 |
| `dev/<username>` | 个人自由开发分支 | 个人自行维护；不得直接作为合并进 `main` 的凭据。 |

## 命名规则：只用 `/` 分层，不用 `-`

**分支名里一律不出现 `-`。** 层级像文件夹一样用 `/` 分隔，每一段只含小写字母与数字；一段里有多个词时用 `_` 连接。
由 `scripts/check-branch-invariants.mjs` 强制检验：
- 任务分支：`task/<issue>/<slug>` 或 `task/<slug>`，例如 `task/support_prime_and_cline`。
- 个人分支：`dev/<username>`，例如 `dev/crosery`。

## 硬不变量

以下两条必须同时成立，由 `scripts/check-branch-invariants.mjs` 强制检验：

1. **`stage` 必须包含 `main`**：`git merge-base --is-ancestor origin/main origin/stage` 为真（即 stage ≥ main）。
2. **`main` 不得领先 `stage`**：任何写入 `main` 的提交都必须已经存在于 `stage`（只允许把 `stage` 快进/合并进 `main`）。

本地核对命令：
```bash
node scripts/check-branch-invariants.mjs
```

## 日常流程

1. 确认分支：`git branch --show-current`（确保在 `stage`）。
2. 开发需求：从 `stage` 检出 `task/<slug>` 或直接在 `stage` 分支工作。
3. 验证通过：运行 `scripts/verify.sh`（包含 `check-branch-invariants.mjs`）。
4. 预发布验收后，需要发稳定版时，才将 `stage` 合入 `main` 并打正式 tag。
