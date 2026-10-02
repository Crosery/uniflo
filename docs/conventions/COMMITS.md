# 提交规范

> Conventional Commits 结构，中文说明，一次提交一个可回滚目的。

状态：`current` · 更新：2026-10-02

## 格式

```text
<type>(<scope>): <中文简述>

为什么改；改变了什么；用什么命令验证、结果如何。
```

- 首行不超过 72 字符，半角冒号，不加句号或 emoji；代码标识符和路径保持英文。
- type：`feat` `fix` `refactor` `perf` `docs` `test` `build` `ci` `chore` `style`（`style` 只指格式）。
- scope 按职责中心选一个：monorepo 用包或服务名（`gateway`、`console`、`web`…），另有 `docs`、`deploy`、`tooling`、`deps`。本项目的 scope 词表：`schema` `core` `search` `adapters`（或具体 harness：`claude` `codex` `omp`…）`gateway` `cli` `docs` `tooling` `ci` `deps`。
- 例：`feat(omp): 按打开的会话文件映射存活进程`；`fix(core): 存活探测移出引擎循环`；`docs(schema): 补充 turn_end.reason 取值说明`。

## 原子性

- 一个提交 = 一个可独立理解、可单独回滚的目的；代码与它的契约、测试、文档在同一提交。
- 机械搬迁与行为改变尽量分开，但不制造不可构建的中间态。
- 破坏性变更用 `type(scope)!:` 或正文 `BREAKING CHANGE:`，写明影响与迁移。
- 消息要带信息量：写清改了什么行为，而不是 `update`、`WIP`、`fix bug`。

## 授权

- 功能或缺陷修复完成、验证通过后，agent 创建一个本地提交。
- 推送、合并、打 tag、发布各自需要用户明确授权；本地提交不代表可以推送。

依据：[Conventional Commits 1.0.0](https://www.conventionalcommits.org/en/v1.0.0/)；中文简述与 scope 词表是本项目约定。
