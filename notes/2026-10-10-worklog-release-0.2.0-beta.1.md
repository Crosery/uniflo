# 2026-10-10 · v0.2.0-beta.1 发布（未完成）

## 状态

- service-expansion（Comet Native Supervisor，9 个子任务）最终验收 65/65 通过，用户接受；已归档并本地合入 `stage`（0163f81），`task/service_expansion` 与 worktree 已删除。
- 用户授权：版本号 0.2.0-beta.1、合入 stage、推送 stage 与 tag `v0.2.0-beta.1`；tag 首次失败后用户同意删除并在修复提交上重建。
- 版本号提交 8837797；之后在 stage 上追加：
  - cce32d2 `fix(core)`：Rust 1.99 clippy `unnecessary_sort_by`；
  - 18cc771 `fix`：Windows 下未使用的 `mut`（`agent.rs` 的 `DirBuilder`）与测试变量（`craft.rs`）；
  - 564fb8b `docs(release)`：release 工作流在 `docs/releases/<tag>.md` 存在时用它作为 Release 正文。
- 远程 tag `v0.2.0-beta.1` 目前指向 cce32d2。**crates.io 与 GitHub Release 都还没有任何产物**（两次 release 都停在 publish 之前的 `scripts/verify.sh`）。

## 两次发布失败

| 运行 | 原因 | 处理 |
|---|---|---|
| release 38030306268（tag @ 8837797） | CI stable 是 Rust 1.99，新增 `clippy::unnecessary_sort_by`（`engine_usage.rs:168`）；本机 stable 是 1.94.1 | cce32d2 修复；本机 `RUSTUP_TOOLCHAIN=1.99.0 scripts/verify.sh --e2e` 全过（e2e 125/0），`cargo +1.99.0 package --workspace --locked` 6 个 crate 打包与校验通过 |
| release 38032110407（tag @ cce32d2） | ubuntu 上 `crates/uniflo-gateway/tests/cleanup.rs` 不稳定：`execute_archives_then_trashes_and_the_archive_stays_usable` 第 428 行归档后会话列表里该 key 计数为 0（期望 1）；同次 `restoring_from_the_trash_brings_the_source_back` 第 271 行还原后子代理会话 404。stage 的 ci 运行 38030842581 里只有前者失败 → 时序相关 | **未修**，见下 |
| ci 38030842581（stage @ cce32d2） | windows-latest clippy：`agent.rs:162` 多余 `mut`、`craft.rs:329` 未使用变量 | 18cc771 修复，待 CI 确认 |

## Linux 清理测试排查记录（未定位）

- macOS 上在 execute 之后插 3 s 延时再断言，测试仍通过，所以不是「重扫后归档会话丢失」这种 macOS 也会出现的问题。
- 归档条目的 `src` 是归档文件路径（`engine_archive.rs::archived_entry`），原路径的删除事件走 `remove_source` 时因 `st.sources` 已无该路径而提前返回；`apply()` 对墓碑路径直接丢弃。注入回收站在 `<root>/trash`，不在 harness 根内。
- 下一步：在 Linux 上复现（本机 OrbStack 未启动；可 `orb start` 后 `docker run --rm -v "$PWD":/src:ro -w /src -e CARGO_TARGET_DIR=/tmp/t rust:1.99 cargo test -p uniflo-gateway --test cleanup`，多跑几次），重点看 inotify 事件在 `retire()` 之后、文件移入回收站期间触发的 `process()` / 重扫路径是否把归档条目覆盖或移除，以及还原时子代理目录的事件顺序。

## 继续发布的步骤

1. 修好 Linux 测试，三平台 ci 全绿（`gh run list --workflow ci.yml`）。
2. 删除远程与本地 tag，在新的 stage HEAD 上重建 `v0.2.0-beta.1` 并推送（用户已同意这种重发方式）。
3. 观察 release：publish（crates.io 6 个 crate）→ build（5 目标）→ release（Pre-release，正文取 `docs/releases/v0.2.0-beta.1.md` 再接自动生成的列表）。
4. 线上验证：`cargo install uniflo --version 0.2.0-beta.1`、`UNIFLO_VERSION=0.2.0-beta.1` 安装脚本、0.1.4 上 `uniflo update --pre`。

## 其他待用户决定

- 7471 测试守护进程仍在运行（真实数据只读、开着全文索引，可执行文件所在的集成 worktree 已删除）：`pkill -f 'uniflo daemon --bind 127.0.0.1:7471'`。
- 测试用全文索引缓存 `~/Library/Caches/uniflo/fts-v1.sqlite`（约 5.6 GB）是否删除。
- 全文索引标签含 Uniflo 版本，每次升级整份重建；是否改为只随格式与 schema 版本变化。
- `scripts/dist-e2e.sh` 用真实 0.1.4 作为旧版会在「终端上运行 setup」一条失败（0.1.4 没有 `setup` 子命令），属预期；分发链路已由 distribution 子任务用同一代码的 0.1.4/0.1.5 构建验证。
