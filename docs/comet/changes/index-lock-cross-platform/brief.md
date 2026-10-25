# Outcome

修复全文索引 `write: database is locked` 错误持续累积，并修复经证据确认的 Windows、Linux、macOS 缺陷；完成跨平台验证、补丁发布与官网和仓库同步。

# Scope

- 全文索引写入、进程内 MCP 回退与守护进程并存时的缓存所有权和查询行为。
- Windows、Linux、macOS 的进程、目录监听、安装升级与终端恢复路径，按可复现证据修复。
- 工作中父会话家族与子代理按本轮进入工作状态的先后固定顺序，持续工作期间不因回复、元数据或缺失 started_at 跳位；退出再进入工作队列排到后面。
- 回归测试、真实程序 smoke、发布说明、官网、仓库、7 个 crate 和 5 个平台发布包。

# Non-goals

不修改 harness 真实会话、数据库和凭据，不删除真实索引来掩盖问题，不覆盖已发布 tag 或 crates.io 版本，不引入新生产依赖，不在未授权时更换本机或远端真实安装。

# Acceptance examples

- 同一缓存目录有守护进程和 MCP/第二索引进程时，查询仍可用，不出现持续竞争写入；已有命中保留，新内容在写者中继续可检索。
- 索引遇到暂态 SQLite 写竞争时，不丢弃已接收的索引内容，也不把失败的 pass 标为已完成。
- Windows 经确认的缺陷有修前失败、修后成功证据，并在 Windows 真实程序或 CI 上验证。
- Linux 经确认的缺陷有修前失败、修后成功证据，并在 Linux 真实程序或 CI 上验证。
- macOS 经确认的缺陷有修前失败、修后成功证据，并在本机真实程序上验证。
- 工作中会话、子代理按本轮 work 进入顺序排列；更新、加载更多、刷新不改变同一工作轮次顺序，idle 后再次 work 排到队列末尾，空闲列表仍按最近活动排列。
- 统一门禁与网页 e2e 全部通过，worklog 区分已验证与未验证，既有搜索、会话与用量路径不回退。
- 用户明确确认新版本后发布补丁版；GitHub Release 的 5 个包与校验和、7 个 crate、官网和仓库版本与发布说明一致。

# Constraints and invariants

wire schema v1 只增不改；依赖单向；harness 只读；测试使用合成数据与隔离 HOME/data/cache；共享工作区由主代理集成，最终统一执行门禁。

# Decisions

- 当前仓库为 stage，开始时工作区干净；0.2.2 已于 2026-10-25 发布，不能覆盖 crates.io 同版本。
- 本机 `lsof` 观察到 daemon pid 58838 与 MCP pid 74239 同时以写模式打开同一个 `fts-v2.sqlite`；这不等同于已证实全部根因，需用隔离回路验证。
- 一个 Native change 统一处理紧密相关的修复和交付，调查可按平台独立进行，不创建 Supervisor 子 change。
- 下一补丁版本提议为 0.2.3，与完整目标在最终确认中一并明确；结构化版本选择超时不作为发布授权。

# Open questions

无额外未决的产品行为问题；补丁版本和完整范围等待最终确认。

# Verification expectations

先建立可独立运行的红绿回路；保留消费者可见行为回归；最终 `RUSTUP_TOOLCHAIN=1.99.0 CARGO_TARGET_DIR=target/clippy-199 scripts/verify.sh --e2e`，另跑隔离 daemon/MCP smoke 与各平台实际路径。发布和线上页面按真实返回核对，不把源码审阅当运行证据。
