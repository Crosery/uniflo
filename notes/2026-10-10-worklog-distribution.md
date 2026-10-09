# 2026-10-10 worklog · distribution

> service-expansion 子任务 distribution：GitHub Releases 发布 5 个目标的预编译包，提供 `install.sh` / `install.ps1` 和 cargo-binstall 元数据，`uniflo update` 按安装方式升级（ADR-0009，修订 ADR-0005）。所有安装和升级都只在临时目录里验证，用本地 HTTP 服务模拟发布地址。

状态：`historical` · 更新：2026-10-10

## 00:20 · 预编译分发与按安装方式升级

### 完成条件（逐条对应 `specs/distribution/spec.md` 的 Scenario）

- **安装脚本成功与校验失败**：
  - 用 `scripts/package.sh` 打出当前平台的 release 包，连同 `SHA256SUMS` 由本地 HTTP 服务提供，`UNIFLO_RELEASE_BASE_URL` 指向它。
  - 以非 TTY 方式运行 `install.sh`：退出码 0，可执行文件位于临时安装目录，`--version` 等于包版本。
  - `install.json` 为 `{method:"binary", target, version, path}`，`path` 是安装后的可执行文件；没有运行 setup，即没有生成 `setup.json`，输出"跳过 uniflo setup"。
  - 把包翻转一个字节后再运行：退出码非 0，输出"校验失败"；安装目录不存在，或已有的可执行文件逐字节不变；不写 `install.json`；`TMPDIR` 里不残留文件。
- **binary 方式自升级**：
  - 用 `install.sh` 安装旧版本，本地发布地址上放新版本包；`UNIFLO_UPDATE_INDEX_URL` 指向本地稀疏索引，以此注入检查结果。
  - 运行 `uniflo update`：可执行文件变成新版本，`--version` 输出新版本号，`install.json` 的 `version` 同步更新。
  - 把 `SHA256SUMS` 改错再升级：退出码非 0，原文件逐字节不变，`version` 不变。
  - 另外补测包缺失（下载失败）和包不是 tar.gz（解包失败）两种情况，结果相同。
- **安装方式识别**：可执行文件位于 `$CARGO_HOME/bin` 或 `~/.cargo/bin` 时 `update --check --json` 返回 `"method":"cargo"`；`install.json` 与当前路径一致时返回 `binary`；两者都不满足时返回 `unknown`，并打印 cargo 与安装脚本两种升级方式。
- **发布工作流与 binstall 元数据**：
  - `release.yml` 能通过 YAML 解析，并有人工核对和 actionlint 两道检查；含 5 个目标、`SHA256SUMS` 生成步骤、上传步骤，`-` tag 加 `--prerelease`。
  - 用 `cargo metadata` 读出 binstall 模板，展开后与工作流的产物名一致。
  - 真实的跨平台构建与上传标"未验证"。
- 安全：不替换 `~/.cargo/bin/uniflo`，不写真实 `install.json`，不运行 `launchctl kickstart/bootout/load`。用户守护进程 `com.crosery.uniflo` 的 pid 与 uptime 在全部验证前后保持连续。

### 改动

- **core**（`crates/uniflo-core/src/install.rs`，新模块）：
  - 安装记录 `InstallRecord`（`<配置目录>/install.json`）与安装方式识别 `detect`，路径比较前先解析符号链接；
  - 发布包命名：`asset_name`、`asset_binary`、`TARGETS`；
  - `listed_sum`：解析 `SHA256SUMS`；
  - `upgrade_binary`：curl 下载 → `sha2` 校验 → tar / Expand-Archive 解包 → `--version` 自检 → `swap`；
  - `swap`：暂存在同目录后改名，分两种方式：`Replace` 直接覆盖，`Aside` 是 Windows 的 `.old` 改名，失败时回滚；
  - `remove_aside`：下次启动时清理 `.old`。
- **core**（`update.rs`）：稀疏索引地址可由 `UNIFLO_UPDATE_INDEX_URL` 覆盖，测试用它注入检查结果。
- **cli**：
  - `update.rs`（新模块）：原 `main.rs` 里的 `update()` 移到这里，改为按安装方式分派：
    - `cargo` 沿用 `cargo install`；
    - `binary` 调用 `upgrade_binary`，再写回 `install.json`；
    - `unknown` 只打印两种升级方式。
    - `--check --json` 在原有字段外加上 `method`。
    - launchd 判断 `launchd()` 通过注入的闭包执行 `launchctl`：只有 `launchctl print` 的 `program` 就是被升级的可执行文件时才 kickstart。
    - 两条成功路径最后都调用一次 `setup::after_update(&exe)`。
  - `setup::after_update` 改为接收可执行文件路径：Linux 上文件被替换后，`current_exe()` 会带 ` (deleted)` 后缀。
  - `main.rs`：Windows 下启动时清理 `.old`。
- **工作流与脚本**：
  - `release.yml`：原 `publish` job 不动；新增 `build` 矩阵（5 个目标）和 `release` job（生成 `SHA256SUMS`、`gh release create`，`-` tag 加 `--prerelease`，附带两份安装脚本）。
  - `scripts/package.sh`：打包，CI 与测试共用。
  - `scripts/install.sh`、`scripts/install.ps1`：安装。
  - `scripts/check-dist.mjs`：检查工作流、binstall 模板、`package.sh` 产物三者一致。
  - `scripts/third-party-notices.mjs`：生成 `THIRD_PARTY_NOTICES.md` 的 Rust crate 一节。
  - `scripts/dist-e2e.sh`：本机 release 包全链路。
  - `scripts/verify.sh` 新增两步：`check-dist.mjs`、`third-party-notices.mjs --check`。
- **binstall**：`crates/uniflo-cli/Cargo.toml` 新增 `[package.metadata.binstall]`，Windows 覆盖为 `.zip`。
- **文档**：
  - 新增 ADR-0009；
  - ADR-0005 加修订说明；
  - 更新 `docs/conventions/BRANCHING.md` 的发布一节；
  - README 的安装一节改为三种方式，升级一节补安装方式表；
  - 更新 `docs/architecture.md`（分发与升级、`install.json`）、`docs/conventions/DEVELOPMENT.md`（模块表、联网说明）、`docs/agents.md`（安装脚本触发 setup）；
  - `AGENTS.md` 新增按任务读的一行和安装 / 升级测试的安全规则。

### 实现时定下的取舍

- **最新版本怎么定**：
  - `install.sh` 读 `<base>/latest/download/SHA256SUMS`，从里面的包名取版本，不调 GitHub API（匿名有速率限制，sh 里也不好解析 JSON）。
  - `uniflo update` 仍以 crates.io 稀疏索引为准。工作流先发 crates.io，再构建二进制，所以中间有几分钟下载会 404；这时 `uniflo update` 中止，原文件不变，已写入 ADR 与 BRANCHING。
- **launchd 只在 `program` 匹配时才 kickstart**：比规格多了这一个条件。服务运行的若是另一份可执行文件，例如 `~/.cargo/bin/uniflo`，重启它也换不成新版本；这个条件同时保证临时安装的测试碰不到用户真实的守护进程。已写进 ADR-0009 的否决项。
- **安装前的 `--version` 自检**：install.sh 和 `upgrade_binary` 都在替换前先运行一次解出的文件，必须输出 `uniflo <版本>`。这能挡住架构不对、包损坏、包名与内容版本不一致。
- **THIRD_PARTY_NOTICES.md**：
  - 内容是编进二进制的全部 194 个 crate（含所有目标平台）的清单，加上它们随包附带的许可证文本，相同文本只列一次，共 113 份，约 400 KB。
  - 生成段落用 `BEGIN/END rust-crates` 标记包住，web-console 的图标说明另起一节，合并时互不覆盖。
  - 有 6 个 crate 没带许可证文件，按声明的许可证列出：objc2 系 3 个，以及 r-efi、valuable、wasip2。

### 验证（命令 → 真实结果）

| 检查 | 命令 | 结果 |
|---|---|---|
| core 单测 | `cargo test -q -p uniflo-core install::` | 5 passed（识别三种情况、5 个包名、SHA256SUMS 解析、`Replace`/`Aside` 交换与清理、记录读写与字段顺序） |
| cli 单测 | `cargo test -q -p uniflo --bin uniflo update::` | 2 passed（launchd：未加载只 print；`program` 匹配才 `kickstart -k gui/501/com.crosery.uniflo`；不匹配、输出无法解析、拿不到 uid 时都不重启；两种升级方式的文案） |
| 分发集成测试 | `cargo test -q -p uniflo --test dist` | 5 passed，约 3.4 s（真实二进制 + `package.sh` + 进程内 HTTP 服务 + `install.sh`；见下表） |
| 一致性检查 | `node scripts/check-dist.mjs` | `release packaging: 5 targets, binstall templates match, aarch64-apple-darwin package layout ok`；把 pkg-url 改成 `{ name }-{ target }-v{ version }` 后 exit 1，列出 5 条不一致，恢复后通过 |
| 许可证清单 | `node scripts/third-party-notices.mjs --check` | `194 crates, 113 license texts, up to date` |
| shell 静态检查 | `shellcheck scripts/package.sh scripts/dist-e2e.sh`；`shellcheck -s sh scripts/install.sh` | 无告警 |
| POSIX sh | `/bin/dash scripts/install.sh --no-setup`（本地服务、临时目录） | exit 0，`uniflo 0.1.4`，`install.json` 正确，`TMPDIR` 空 |
| YAML 解析 | `python3 -c "import yaml; …"` | jobs `publish`, `build`, `release`；matrix 目标与规格的 5 个一致 |
| actionlint | actionlint 1.7.7，下载到 `/tmp` 的独立目录，sha256 与官方 checksums 一致：`actionlint -shellcheck $(command -v shellcheck) .github/workflows/*.yml` | 只有 2 条 SC2129 style，都在没改动的 "Parse tag" 步骤；对 `HEAD` 版本的 release.yml 跑同样得到这 2 条。`-ignore SC2129` 后 exit 0 |
| binstall 元数据 | `cargo metadata --format-version 1 --no-deps` | `pkg-url = "{ repo }/releases/download/v{ version }/{ name }-{ version }-{ target }.tar.gz"`、`bin-dir = "{ name }-{ version }-{ target }/{ bin }{ binary-ext }"`、`pkg-fmt = "tgz"`，Windows 覆盖为 `.zip` 与 `zip`；`repository = https://github.com/Crosery/uniflo` |
| install.ps1 | PowerShell 7.4.6 for macOS，下载到 `/tmp` 的独立目录，sha256 与官方 `hashes.sha256` 一致：`Parser::ParseFile` | 0 个解析错误 |
| install.ps1 功能（macOS 上的 pwsh） | 本地服务提供 `uniflo-9.9.9-x86_64-pc-windows-msvc.zip`（内含一个假 `uniflo.exe`），设 `PROCESSOR_ARCHITECTURE=AMD64` | 篡改包：exit 1、"校验失败…已中止，没有安装任何文件"，不建安装目录、不写 `install.json`。正常包：exit 0，从 latest 的 `SHA256SUMS` 得到版本 9.9.9，`Get-FileHash` 校验与 `Expand-Archive` 都通过；`install.json` 首字节是 `{`（无 BOM），四个字段正确 |
| release 构建 | `cargo build --release --locked -p uniflo --target aarch64-apple-darwin` | 48 s，`uniflo 0.1.4` |
| 新版本构建 | 把工作区 rsync 到 `/tmp/uniflo-dist-next/src`，只把 `[workspace.package] version` 改成 0.1.5，`CARGO_TARGET_DIR=/tmp/uniflo-dist-next/target cargo build --release -p uniflo --target aarch64-apple-darwin` | 48 s，`uniflo 0.1.5` |
| release 包全链路 | `scripts/dist-e2e.sh /tmp/uniflo-dist-bins/uniflo-0.1.4 /tmp/uniflo-dist-bins/uniflo-0.1.5`（`python3 -m http.server`，空闲端口 65307） | 8 条全部 PASS（见下表），退出时服务已停，临时目录已删 |
| 其他平台类型检查 | `RUSTFLAGS="-D warnings" cargo check -p uniflo-core --all-targets --target x86_64-pc-windows-msvc`，另跑一次 `--target x86_64-unknown-linux-gnu` | 都 exit 0，`Swap::Aside`、`curl.exe`、`Expand-Archive` 分支都通过编译。CLI crate 无法本机交叉检查，因为 bundled SQLite 需要目标平台的 C 工具链。改为在临时副本里把 `update.rs` 的 `target_os = "macos"` 换成 `"linux"`，再跑 `cargo clippy -p uniflo --all-targets -- -D warnings`：exit 0，即非 macOS 分支能编译且没有告警 |
| 统一验收 | `scripts/verify.sh`（没动网关和网页演示，不加 `--e2e`） | exit 0，`verify: all checks passed`。依次通过：rustfmt、clippy `-D warnings`、25 个适配器 feature 单独编译、branch invariants（只有 comet 子分支命名的既有警告）、新增的 release packaging 与 third-party notices 两步、24 个测试套件（292 个测试：291 passed、1 ignored 为既有、0 failed）、release 构建与 7399 端口上的守护进程冒烟 |
| 用户守护进程未受影响 | `launchctl print gui/$(id -u)/com.crosery.uniflo`（只读）+ `GET 127.0.0.1:7311/v1/health` | `state = running`、`program = /Users/crosery/.cargo/bin/uniflo`、`pid = 772`、`uptime_ms = 134560949`（约 37 h，没有被重启）；没有残留的 `http.server` 进程 |

`dist-e2e.sh` 的真实输出（节选）：

```text
PASS  install.sh (non-TTY): uniflo 0.1.4 installed, install.json written, setup skipped, temp dir cleaned
PASS  install.sh (tampered package): exit 1, no install dir, no install.json, temp dir cleaned
      uniflo-install: 错误：uniflo-0.1.4-aarch64-apple-darwin.tar.gz 校验失败：SHA-256 为 af7c0b6f…，SHA256SUMS 记录的是 ba0f72b8…。已中止，没有安装任何文件
PASS  install.sh (TTY): uniflo setup ran (answered 'skip', recorded asked=true in setup.json)
PASS  install.sh (TTY, --no-setup): setup skipped
PASS  install.sh (TTY, UNIFLO_NO_SETUP=1): setup skipped
PASS  uniflo update (wrong checksum): non-zero exit, executable and install.json unchanged
      Error: 升级失败，…/a/bin/uniflo 未改动
      Caused by: checksum mismatch for uniflo-0.1.5-aarch64-apple-darwin.tar.gz: got d30ba39f…, SHA256SUMS lists 0000…
PASS  uniflo update (binary): replaced by uniflo 0.1.5, install.json version=0.1.5, launchd service untouched
      launchd 服务 com.crosery.uniflo 运行的是 /Users/crosery/.cargo/bin/uniflo，不是刚升级的 …/a/bin/uniflo，未重启。
PASS  update --check --json: method cargo=cargo, binary=binary, unknown=unknown; unknown prints both ways (stderr)
dist-e2e: all checks passed
```

TTY 场景用 macOS 的 `script -q /dev/null` 给安装脚本分配 pty，stdin 喂入 "4"（跳过）。PATH 只含系统目录，HOME、`UNIFLO_HOME` 和配置目录都是临时目录，所以 setup 检测不到任何 harness，也不会写 harness 配置。

### Scenario → 证据

| Scenario | 证据 | 结论 |
|---|---|---|
| 安装脚本成功与校验失败 | `dist.rs::install_script_installs_records_and_skips_setup_off_a_terminal`、`install_script_aborts_on_a_checksum_mismatch_and_leaves_nothing`、`install_script_follows_uniflo_home_for_the_record`；`dist-e2e.sh` 用 release 包的前 5 条 PASS | PASS |
| binary 方式自升级 | `dist.rs::binary_install_upgrades_itself_and_failures_change_nothing`（校验值错误、下载失败、解包失败都不改动，成功后版本与记录都更新）；`dist-e2e.sh` 的两条 `uniflo update` PASS（真实的 0.1.4 → 0.1.5） | PASS |
| 安装方式识别 | `install::tests::method_follows_record_then_cargo_dir`；`dist.rs::update_reports_the_install_method`（`$CARGO_HOME/bin`、`~/.cargo/bin`、install.json、unknown 四种情况，unknown 提示两种方式，`uniflo update` 不改文件）；`dist-e2e.sh` 最后一条 PASS | PASS |
| 发布工作流与 binstall 元数据 | YAML 解析、actionlint、`check-dist.mjs`（含反例）、`cargo metadata` 输出、`install::tests::asset_names_per_target`。核对项：<br>- 5 个目标；<br>- `sha256sum uniflo-* > SHA256SUMS` 前检查包数为 5；<br>- 上传：`gh release create "$TAG" dist/*`；<br>- 预发布：`[[ "$TAG" == *-* ]]` → `--prerelease`，在 release job 里传入；<br>- crates.io 步骤一字未改。 | PASS（静态检查） |
| 真实的跨平台构建与上传 | 需要推送 tag，由 CI 执行；推送 tag 需要用户授权 | 未验证 |

### 未验证与剩余风险

- **跨平台 CI 未验证**：真实的 5 目标构建与 Release 上传需要推送 tag。具体有：
  - `ubuntu-24.04-arm` 只对公开仓库免费；
  - musl 目标用 `musl-gcc` 编 bundled SQLite；
  - Windows 上用 7z 打 zip。
- **Linux 未验证**：install.sh 在 Linux（dash、`sha256sum`、`XDG_CONFIG_HOME`）上的行为本机无法实测。`ci.yml` 的 ubuntu job 会跑 `dist.rs`，需要推送后才能看到结果。
- **Windows 未验证**：
  - 真实 Windows 上的 `install.ps1`，以及 `uniflo update` 的 `.old` 改名与下次启动清理；
  - `Expand-Archive` 解包路径与 `%APPDATA%` 配置目录。
  - 这部分逻辑已有跨平台单测和 macOS 上的 pwsh 功能运行覆盖。
- **launchd kickstart 未验证**：真正执行 kickstart 的那条路径没有跑过，需用户授权后人工核对。本次只用注入的 `launchctl` 断言了将要执行的命令，并在真实机器上确认临时安装不会触发它。
- **`cargo install` 升级路径未验证**：本次没有真实执行，以免替换 `~/.cargo/bin/uniflo`。代码沿用原实现，未改动。
- **完整性只靠同源校验**：`SHA256SUMS` 与发布包来自同一个 Release，只防传输损坏和错包，不防仓库或发布凭据被攻破（ADR-0009 已写明复议触发）。
