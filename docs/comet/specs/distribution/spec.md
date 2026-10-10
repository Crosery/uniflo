# distribution 规格

Uniflo 的主渠道仍然是 crates.io（`cargo install uniflo`）。本规格新增 GitHub Releases 预编译二进制作为附加渠道，同时提供安装脚本和 cargo-binstall 元数据，并让 `uniflo update` 按安装方式选择升级路径。ADR-0009 修订 ADR-0005。

## 发布产物

- `.github/workflows/release.yml` 在现有 tag 触发流程中新增构建矩阵，产出 5 个目标：
  - `aarch64-apple-darwin`
  - `x86_64-apple-darwin`
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `x86_64-pc-windows-msvc`
- 每个目标打包为 `uniflo-<version>-<target>.tar.gz`（Windows 为 `.zip`），包内含可执行文件、`LICENSE`、`THIRD_PARTY_NOTICES.md`。
- 生成一份汇总的 `SHA256SUMS`，与各包一起上传到对应 tag 的 GitHub Release。
- 预发布 tag（含 `-`）的产物标记为 prerelease。
- crates.io 的发布流程保持不变。
- `uniflo-cli` 的 `Cargo.toml` 增加 `[package.metadata.binstall]`，指向上述包名和目录结构。

## 安装脚本

- **`scripts/install.sh`**（macOS / Linux）：
  - 识别 OS 与架构，选择对应目标。
  - 版本默认取最新正式版，可通过 `UNIFLO_VERSION` 指定。
  - 下载包和 `SHA256SUMS`，用 `shasum -a 256` 或 `sha256sum` 校验，不一致时中止，且不留下任何文件。
  - 安装到 `UNIFLO_INSTALL_DIR`，默认 `~/.local/bin`。安装目录不在 PATH 中时打印提示。
  - 写入 `<配置目录>/install.json`：`{method: "binary", target, version, path}`。
  - 最后，如果处于 TTY 且没有设置 `--no-setup` 或 `UNIFLO_NO_SETUP=1`，运行 `uniflo setup`。
  - 下载根地址可通过 `UNIFLO_RELEASE_BASE_URL` 覆盖，便于测试。
- **`scripts/install.ps1`**（Windows）：行为相同。默认安装到 `%LOCALAPPDATA%\Programs\uniflo`，用 `Get-FileHash` 校验。
- README 的安装一节给出三种方式：
  - `cargo install uniflo`（推荐给 Rust 用户）
  - 一行安装脚本
  - `cargo binstall uniflo`

## 按安装方式升级

- `uniflo update` 先判断安装方式：
  - 存在 `install.json`，且记录的路径就是当前可执行文件：视为 binary。
  - 当前可执行文件位于 `$CARGO_HOME/bin` 或 `~/.cargo/bin` 下：视为 cargo。
  - 其他情况：视为 unknown。
- 各方式的处理：
  - **cargo**：沿用现有的 `cargo install uniflo --force --version <v>`。
  - **binary**：
    1. 用系统 `curl` 下载当前目标的包和 `SHA256SUMS`。
    2. 用 `sha2` 校验。
    3. 解包到临时目录。
    4. 原子替换可执行文件。Windows 上先把正在运行的文件改名为 `.old` 再放入新文件，`.old` 在下次启动时清理。
  - **unknown**：打印两种升级方式，不做任何改动。
- `--pre` 语义不变，只在显式指定时安装预发布版。
- 升级成功后的后续动作：
  - macOS 上已加载 launchd 服务时，执行 `launchctl kickstart -k` 重启守护进程。
  - 随后进入 agent-access 规格中定义的 setup 收尾流程。
- 校验失败、下载失败、解包失败时，原可执行文件保持不变，退出码不为 0，并给出原因。

## 文档

- ADR-0009（预编译分发与自升级）记录以下内容：
  - 不做签名和公证的理由：经 curl 下载的命令行程序不会带隔离属性。
  - 不做 Homebrew。
  - 校验策略。
- `docs/conventions/BRANCHING.md` 的发布一节同步更新。

### Scenario: 安装脚本成功与校验失败
- GIVEN 本机构建的当前平台 release 包与 `SHA256SUMS`，由本地 HTTP 服务器模拟发布地址（通过 `UNIFLO_RELEASE_BASE_URL` 指向它）；一个临时的安装目录
- WHEN 以非 TTY 方式运行 `install.sh`
- THEN 可执行文件被安装到临时目录，`uniflo --version` 与包版本一致，`install.json` 已写入，未运行 setup
- AND 篡改包中一个字节后再运行，脚本中止，退出码不为 0，安装目录中不留下任何文件

### Scenario: binary 方式自升级
- GIVEN 用 install.sh 安装在临时目录的旧版本，以及本地模拟发布地址上的新版本包
- WHEN 运行 `uniflo update`（更新检查的结果通过测试注入，指向新版本）
- THEN 可执行文件被替换为新版本，`install.json` 中的 version 已更新
- AND 把新包的校验值改错后再升级，原可执行文件保持不变，退出码不为 0

### Scenario: 安装方式识别
- GIVEN 三种情况：可执行文件位于 `~/.cargo/bin`；有与当前路径匹配的 `install.json`；两者都不满足
- WHEN 运行 `uniflo update --check --json`
- THEN 返回的 `method` 分别为 `cargo`、`binary`、`unknown`
- AND unknown 时打印两种升级方式

### Scenario: 发布工作流与 binstall 元数据
- GIVEN 修改后的 `release.yml` 与 `Cargo.toml`
- WHEN 用 `actionlint` 检查工作流（本机未安装时改用 YAML 解析加人工核对），并用 `cargo metadata` 读取 binstall 元数据
- THEN 工作流语法有效，包含 5 个目标、`SHA256SUMS` 生成步骤、上传步骤，预发布标记逻辑正确
- AND binstall 元数据中的包名模板与工作流的产物名一致
- AND 真实的跨平台构建与上传只能在推送 tag 后由 CI 验证。在本子任务中标为"未验证"，推送 tag 需要用户授权
