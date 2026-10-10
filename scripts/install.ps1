# Install a prebuilt uniflo release on Windows (ADR-0009): download the package and SHA256SUMS,
# verify with Get-FileHash, install, record <config dir>\install.json for `uniflo update`, then
# offer `uniflo setup` when run from an interactive console.
#
#   irm https://github.com/Crosery/uniflo/releases/latest/download/install.ps1 | iex
#   powershell -ExecutionPolicy Bypass -File install.ps1 --no-setup
#
# Environment:
#   UNIFLO_VERSION           version to install (default: the newest stable release)
#   UNIFLO_INSTALL_DIR       where uniflo.exe goes (default: %LOCALAPPDATA%\Programs\uniflo)
#   UNIFLO_NO_SETUP=1        never run `uniflo setup` (same as --no-setup)
#   UNIFLO_RELEASE_BASE_URL  release root (default: https://github.com/Crosery/uniflo/releases)
#   UNIFLO_CONFIG_DIR, UNIFLO_HOME  where install.json goes, exactly as for uniflo itself

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    function Say([string]$m) { Write-Host "uniflo-install: $m" }
    # throw, not exit: under `irm | iex` exit would close the user's PowerShell window.
    function Fail([string]$m) { throw "uniflo-install: 错误：$m" }
    function Fetch([string]$url, [string]$out) {
        try { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out }
        catch { Fail "下载失败：$url（$($_.Exception.Message)）" }
    }

    $base = 'https://github.com/Crosery/uniflo/releases'
    if ($env:UNIFLO_RELEASE_BASE_URL) { $base = $env:UNIFLO_RELEASE_BASE_URL.TrimEnd('/') }
    $noSetup = ($args -contains '--no-setup') -or ($args -contains '-NoSetup') -or
        ($env:UNIFLO_NO_SETUP -and $env:UNIFLO_NO_SETUP -ne '0')

    # ARM64 Windows runs the x64 build under emulation.
    if ($env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'ARM64')) {
        Fail "没有 $env:PROCESSOR_ARCHITECTURE 架构的预编译包，可改用 cargo install uniflo"
    }
    $target = 'x86_64-pc-windows-msvc'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('uniflo-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $version = "$env:UNIFLO_VERSION".TrimStart('v')
        if (-not $version) {
            # The newest stable release's SHA256SUMS names its packages, and so its version.
            $latest = Join-Path $tmp 'latest-SHA256SUMS'
            Fetch "$base/latest/download/SHA256SUMS" $latest
            $pattern = '^[0-9a-fA-F]{64}\s+\*?uniflo-(.+)-' + [regex]::Escape($target) + '\.zip$'
            $hit = Get-Content -LiteralPath $latest | Where-Object { $_ -match $pattern } | Select-Object -First 1
            if (-not $hit) { Fail "最新发布里没有 $target 的包" }
            $null = $hit -match $pattern
            $version = $Matches[1]
        }

        $pkg = "uniflo-$version-$target.zip"
        $pkgPath = Join-Path $tmp $pkg
        $sumsPath = Join-Path $tmp 'SHA256SUMS'
        Say "下载 uniflo $version（$target）"
        Fetch "$base/download/v$version/$pkg" $pkgPath
        Fetch "$base/download/v$version/SHA256SUMS" $sumsPath
        $want = $null
        foreach ($line in Get-Content -LiteralPath $sumsPath) {
            if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $Matches[2].Trim() -eq $pkg) {
                $want = $Matches[1].ToLowerInvariant()
                break
            }
        }
        if (-not $want) { Fail "SHA256SUMS 里没有 $pkg" }
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $pkgPath).Hash.ToLowerInvariant()
        if ($got -ne $want) {
            Fail "$pkg 校验失败：SHA-256 为 $got，SHA256SUMS 记录的是 $want。已中止，没有安装任何文件"
        }

        $unpacked = Join-Path $tmp 'unpacked'
        Expand-Archive -LiteralPath $pkgPath -DestinationPath $unpacked -Force
        $new = Join-Path $unpacked "uniflo-$version-$target\uniflo.exe"
        if (-not (Test-Path -LiteralPath $new -PathType Leaf)) { Fail "$pkg 里没有 uniflo-$version-$target\uniflo.exe" }
        $reported = ((& $new --version) | Out-String).Trim()
        if ($reported -ne "uniflo $version") { Fail "解出的 uniflo.exe 无法运行或版本不符（输出：`"$reported`"）" }

        $dir = if ($env:UNIFLO_INSTALL_DIR) { $env:UNIFLO_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\uniflo' }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $dir = (Resolve-Path -LiteralPath $dir).ProviderPath
        $dest = Join-Path $dir 'uniflo.exe'
        # A running uniflo.exe cannot be overwritten but can be renamed; uniflo deletes the .old on its next start.
        $staged = Join-Path $dir ".uniflo-new-$PID.exe"
        Copy-Item -LiteralPath $new -Destination $staged -Force
        try {
            if (Test-Path -LiteralPath $dest) {
                Remove-Item -LiteralPath "$dest.old" -Force -ErrorAction SilentlyContinue
                Move-Item -LiteralPath $dest -Destination "$dest.old" -Force
            }
            Move-Item -LiteralPath $staged -Destination $dest -Force
        } catch {
            Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
            if (-not (Test-Path -LiteralPath $dest) -and (Test-Path -LiteralPath "$dest.old")) {
                Move-Item -LiteralPath "$dest.old" -Destination $dest -Force
            }
            Fail "无法写入 ${dest}：$($_.Exception.Message)"
        }

        # Same location as uniflo_core::paths::config_dir(): UNIFLO_CONFIG_DIR as is, otherwise the
        # roaming AppData directory, re-rooted under UNIFLO_HOME at the same profile-relative path.
        if ($env:UNIFLO_CONFIG_DIR) {
            $cfg = $env:UNIFLO_CONFIG_DIR
        } else {
            $roaming = [Environment]::GetFolderPath('ApplicationData')
            if ($env:UNIFLO_HOME) {
                $profileDir = [Environment]::GetFolderPath('UserProfile')
                if ($roaming.StartsWith($profileDir + '\', [StringComparison]::OrdinalIgnoreCase)) {
                    $roaming = Join-Path $env:UNIFLO_HOME $roaming.Substring($profileDir.Length + 1)
                } else {
                    $roaming = Join-Path $env:UNIFLO_HOME '.config'
                }
            }
            $cfg = Join-Path $roaming 'uniflo'
        }
        New-Item -ItemType Directory -Force -Path $cfg | Out-Null
        $json = [ordered]@{ method = 'binary'; target = $target; version = $version; path = $dest } | ConvertTo-Json
        # serde_json rejects a byte-order mark, which Windows PowerShell's UTF8 encoding writes.
        $record = Join-Path $cfg 'install.json'
        [IO.File]::WriteAllText("$record.tmp", $json + "`n", (New-Object Text.UTF8Encoding $false))
        Move-Item -LiteralPath "$record.tmp" -Destination $record -Force

        Say "已安装 $((& $dest --version | Out-String).Trim()) → $dest"
        $onPath = ($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }
        if (-not $onPath) {
            Say "$dir 不在 PATH 中。在 PowerShell 里运行下面这行后重开终端："
            Write-Host "    [Environment]::SetEnvironmentVariable('Path', '$dir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        }

        if (-not $noSetup -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected) {
            Say '运行 uniflo setup：把 MCP / Skill 接入本机 agent（之后可用 uniflo setup --uninstall 撤销）'
            & $dest setup
            if ($LASTEXITCODE -ne 0) { Say 'uniflo setup 没有完成，之后可以随时运行 uniflo setup' }
        } else {
            Say '跳过 uniflo setup；之后可运行 uniflo setup 把 MCP / Skill 接入本机 agent'
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
} @args
