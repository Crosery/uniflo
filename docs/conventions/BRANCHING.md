# 分支模型规范

> 只有 `main`（正式稳定版）与 `stage`（预发布与动态集成分支）两条长期分支；task 分支合并后必须立即删除，任何操作前先确认当前分支。
> 规范参考：`/Users/crosery/work_file/geek_main/docs/conventions/BRANCHING.md`。

状态：`current` · 更新：2026-10-10

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

推送 `v*` tag 触发发布流水线，按 tag 名称路由发布通道。流水线分三个 job，顺序固定：

1. **`publish`**：跑 `scripts/verify.sh` 全部门禁，通过后按拓扑顺序把 6 个 crate 发布到 crates.io（`scripts/publish-crates.sh`）。crates.io 是主渠道，先发，不等二进制。
2. **`build`**：5 个目标并行构建预编译二进制，见 ADR-0009：
   - `aarch64-apple-darwin`、`x86_64-apple-darwin`
   - `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl`
   - `x86_64-pc-windows-msvc`

   每个目标用 `scripts/package.sh` 打成 `uniflo-<version>-<target>.tar.gz`（Windows 为 `.zip`），包内含可执行文件、`LICENSE`、`THIRD_PARTY_NOTICES.md`。tag 版本与 `Cargo.toml` 不一致时失败。
3. **`release`**：5 个包都到齐后生成 `SHA256SUMS`，连同 `install.sh`、`install.ps1` 一起上传到该 tag 的 GitHub Release。任一目标失败都不建 Release，修复后重跑失败的 job。

| 通道 | tag 示例 | crates.io | GitHub Release | 用户安装 |
|---|---|---|---|---|
| 正式稳定版 | `v0.1.5` | 正式版本 | 正式 Release（`latest` 指向它） | `cargo install uniflo`；一行安装脚本；`cargo binstall uniflo` |
| 预发布 / Beta | `v0.1.5-rc.1`、`v0.1.5-beta.1` | 带后缀的预发布版本 | 标记为 Pre-release（tag 含 `-`），不会成为 `latest` | `cargo install uniflo --version 0.1.5-rc.1`；`UNIFLO_VERSION=0.1.5-rc.1` 加安装脚本 |

- 发布前本地自检：
  - `node scripts/check-dist.mjs` 检查工作流目标、binstall 模板与包名是否一致；
  - `node scripts/third-party-notices.mjs --check` 检查第三方许可证清单是否过期（两者都在 `scripts/verify.sh` 中）。
  - 依赖有变化时，先运行 `node scripts/third-party-notices.mjs` 重新生成。
- 当前平台的安装与自升级全链路可在本机复现：`scripts/dist-e2e.sh <旧版 uniflo> <新版 uniflo>`。真实的跨平台构建与上传只能在推送 tag 后由 CI 验证，推送 tag 需要用户授权。
- crates.io 先于 Release 资产发布，所以中间有几分钟窗口：crates.io 已有新版本，但 Release 资产还没传完。这段时间里 binary 安装的 `uniflo update` 会因下载失败而中止，可执行文件不受影响，稍后重试即可。

*注：GitHub 仓库中需要在 `Settings -> Secrets and variables -> Actions` 中配置 `CARGO_REGISTRY_TOKEN` 密钥；Release 上传使用工作流自带的 `GITHUB_TOKEN`（`contents: write`）。*
