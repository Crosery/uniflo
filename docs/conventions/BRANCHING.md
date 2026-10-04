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

## 自动化发版与 CI/CD 机制 (`.github/workflows/release.yml`)

项目已接入 GitHub Actions 自动化发包与发布流水线，基于推送的 Git Tag 名称自动路由发布通道：

- **正式稳定版发布**：
  - **触发条件**：打纯数字 SemVer tag（如 `git tag v0.1.2` 并推送）。
  - **行为**：CI 自动运行全部测试门禁，通过后自动将全套 6 个 crate 发布为正式稳定版至 crates.io，并在 GitHub 创建正式 Release。
  - **用户安装**：全局用户执行 `cargo install uniflo` 默认安装最新的正式稳定版。

- **预发布 / Beta 版发布**：
  - **触发条件**：打带预发布后缀的 tag（如 `git tag v0.1.2-rc.1` 或 `v0.1.2-beta.1` 并推送）。
  - **行为**：CI 自动识别为预发布通道，将带后缀的预发布版本发布到 crates.io，并在 GitHub 标记创建 Pre-release。
  - **用户安装**：全球用户若需体验最新的预发布 beta 版，执行 `cargo install uniflo --version 0.1.2-beta.1` 即可体验。

*注：GitHub 仓库中需要在 `Settings -> Secrets and variables -> Actions` 中配置 `CARGO_REGISTRY_TOKEN` 密钥。*
