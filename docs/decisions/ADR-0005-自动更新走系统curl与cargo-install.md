# ADR-0005 · 自动更新走系统 curl 与 cargo install

> 更新检查用系统 `curl` 拉 crates.io 稀疏索引（一次 HTTPS GET），安装用 `cargo install uniflo --force`；不引入任何 HTTP/TLS/semver 依赖。

状态：`accepted` · 更新：2026-10-06

## 背景

v0.1.3 要能发现并安装新版本。约束：依赖树里没有任何 HTTP 客户端、TLS 或 semver crate（`ureq`/`rustls`/`semver` 均未使用）；新增生产依赖需要用户批准；守护进程必须零崩溃路径；本机 macOS 与 Windows 测试机都自带 `curl`（Windows 10 1803+）且都装了 rustup/cargo。

## 决策

- 检查：`crates/uniflo-core/src/update.rs` 用 `std::process::Command` 调系统 `curl -fsS --max-time 15`，GET `https://index.crates.io/un/if/uniflo`（NDJSON，逐行解析 `vers`，跳过 yanked），比较用自写的三位号 + 预发布 rank。与 `core::procs` 调 `ps`/`lsof` 同一先例。
- 时机：`uniflo daemon` 默认每小时一次 + 启动后一次（`EngineOptions::update_check`，`--no-update-check` 关闭）；结果写进 `Stats::update`，经 `/v1/stats` 与 `/v1/health`（`update_available`/`latest_version`，可选字段，非 wire schema）暴露。
- 安装：`uniflo update` 执行 `cargo install uniflo --force`；macOS 上若 launchd 服务已加载则 `launchctl kickstart -k` 重启守护进程，其他平台打印重启指引。
- 失败语义：任何网络/curl 缺失/解析失败都落进 `UpdateInfo::error`，`available=false`，不影响索引主链路。

## 否决项

- 加 `ureq`+`rustls`：新生产依赖（体积、审计面、发布门槛），为一个 GET 不值。
- GitHub Releases API：还要二进制资产分发方案（跨 3 平台 ×2 target 打包上传），发布流程现在只发 crates.io。
- 完全离线手动（用户自己 `cargo install`）：发现不了新版本，等于没做。

## 后果

- 守护进程出现出站 HTTPS 请求（仅版本索引，无会话内容、无标识符，UA 只含版本号）；隐私文档与 README 已写明可用 `--no-update-check` 关闭。
- `uniflo update` 假设安装方式是 cargo install；源码 `--path` 本地构建的开发者不受管，照常 git pull + cargo install。
- 版本比较只支持 `X.Y.Z[-pre]`，与本仓发布规范一致。

## 复议触发

要分发独立二进制（不依赖 cargo）或需要签名校验时，再引入下载器与校验依赖。
