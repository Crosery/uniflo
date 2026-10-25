# Install a prebuilt uniflo release on Windows (ADR-0009): download the package and SHA256SUMS,
# verify with Get-FileHash, install, record <config dir>\install.json for `uniflo update`, then
# offer `uniflo setup` when run from an interactive console.
#
#   irm https://github.com/Crosery/uniflo/releases/latest/download/install.ps1 | iex
#   powershell -ExecutionPolicy Bypass -File install.ps1 --no-setup
#
# Environment:
#   UNIFLO_LANG=zh|en        message language (default: the system language)
#   UNIFLO_VERSION           version to install (default: the newest stable release)
#   UNIFLO_INSTALL_DIR       where uniflo.exe goes (default: %LOCALAPPDATA%\Programs\uniflo)
#   UNIFLO_NO_SETUP=1        never run `uniflo setup` (same as --no-setup)
#   UNIFLO_RELEASE_BASE_URL  release root (default: https://github.com/Crosery/uniflo/releases)
#   UNIFLO_CONFIG_DIR, UNIFLO_HOME  where install.json goes, exactly as for uniflo itself

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # Keep this file ASCII: Windows PowerShell 5.1 reads a BOM-less script run with -File in the
    # ANSI code page, and `irm` decodes the release asset (application/octet-stream) as Latin-1, so
    # literal Chinese breaks parsing or prints mojibake. Chinese messages are XXXX-style unicode
    # escapes expanded here; English messages are plain ASCII and need no escaping.
    function T([string]$s) { [regex]::Unescape($s) }

    # Message language: the first of these that is set and non-empty decides, same rule as the
    # CLI (crates/uniflo-cli/src/i18n.rs) -- UNIFLO_LANG, LC_ALL, LC_MESSAGES, the desktop
    # language, then LANG. A value starting with "zh" (any case) means Chinese; anything else,
    # including unset, means English.
    function Get-DesktopLanguage {
        try { (Get-UICulture).Name } catch { $null }
    }
    function Get-FirstLangTag {
        foreach ($v in @($env:UNIFLO_LANG, $env:LC_ALL, $env:LC_MESSAGES, (Get-DesktopLanguage), $env:LANG)) {
            if ($v -and $v.Trim()) { return $v.Trim() }
        }
        return $null
    }
    $langTag = Get-FirstLangTag
    $isZh = $false
    if ($langTag) { $isZh = ($langTag -imatch '^zh') }
    # Pick <zh> <en>: the message in the detected language.
    function Pick([string]$Zh, [string]$En) { if ($isZh) { T $Zh } else { $En } }

    function Say([string]$m) { Write-Host "uniflo-install: $m" }
    # throw, not exit: under `irm | iex` exit would close the user's PowerShell window.
    function Fail([string]$m) {
        $prefix = if ($isZh) { T 'uniflo-install: \u9519\u8bef\uff1a' } else { 'uniflo-install: error: ' }
        throw ($prefix + $m)
    }
    function Fetch([string]$url, [string]$out) {
        try { Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $out }
        catch { Fail ((Pick '\u4e0b\u8f7d\u5931\u8d25\uff1a{0}\uff08{1}\uff09' 'download failed: {0} ({1})') -f $url, $_.Exception.Message) }
    }

    $base = 'https://github.com/Crosery/uniflo/releases'
    if ($env:UNIFLO_RELEASE_BASE_URL) { $base = $env:UNIFLO_RELEASE_BASE_URL.TrimEnd('/') }
    $noSetup = ($args -contains '--no-setup') -or ($args -contains '-NoSetup') -or
        ($env:UNIFLO_NO_SETUP -and $env:UNIFLO_NO_SETUP -ne '0')

    # ARM64 Windows runs the x64 build under emulation.
    if ($env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'ARM64')) {
        Fail ((Pick '\u6ca1\u6709 {0} \u67b6\u6784\u7684\u9884\u7f16\u8bd1\u5305\uff0c\u53ef\u6539\u7528 cargo install uniflo' 'no prebuilt package for the {0} architecture; use cargo install uniflo instead') -f $env:PROCESSOR_ARCHITECTURE)
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
            if (-not $hit) { Fail ((Pick '\u6700\u65b0\u53d1\u5e03\u91cc\u6ca1\u6709 {0} \u7684\u5305' 'the latest release has no package for {0}') -f $target) }
            $null = $hit -match $pattern
            $version = $Matches[1]
        }

        $pkg = "uniflo-$version-$target.zip"
        $pkgPath = Join-Path $tmp $pkg
        $sumsPath = Join-Path $tmp 'SHA256SUMS'
        Say ((Pick '\u4e0b\u8f7d uniflo {0}\uff08{1}\uff09' 'downloading uniflo {0} ({1})') -f $version, $target)
        Fetch "$base/download/v$version/$pkg" $pkgPath
        Fetch "$base/download/v$version/SHA256SUMS" $sumsPath
        $want = $null
        foreach ($line in Get-Content -LiteralPath $sumsPath) {
            if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $Matches[2].Trim() -eq $pkg) {
                $want = $Matches[1].ToLowerInvariant()
                break
            }
        }
        if (-not $want) { Fail ((Pick 'SHA256SUMS \u91cc\u6ca1\u6709 {0}' 'SHA256SUMS has no entry for {0}') -f $pkg) }
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $pkgPath).Hash.ToLowerInvariant()
        if ($got -ne $want) {
            Fail ((Pick '{0} \u6821\u9a8c\u5931\u8d25\uff1aSHA-256 \u4e3a {1}\uff0cSHA256SUMS \u8bb0\u5f55\u7684\u662f {2}\u3002\u5df2\u4e2d\u6b62\uff0c\u6ca1\u6709\u5b89\u88c5\u4efb\u4f55\u6587\u4ef6' '{0} failed verification: SHA-256 is {1}, SHA256SUMS records {2}. Aborted, nothing was installed') -f $pkg, $got, $want)
        }

        $unpacked = Join-Path $tmp 'unpacked'
        Expand-Archive -LiteralPath $pkgPath -DestinationPath $unpacked -Force
        $new = Join-Path $unpacked "uniflo-$version-$target\uniflo.exe"
        if (-not (Test-Path -LiteralPath $new -PathType Leaf)) { Fail ((Pick '{0} \u91cc\u6ca1\u6709 {1}' '{0} has no {1}') -f $pkg, "uniflo-$version-$target\uniflo.exe") }
        $reported = ((& $new --version) | Out-String).Trim()
        if ($reported -ne "uniflo $version") { Fail ((Pick '\u89e3\u51fa\u7684 uniflo.exe \u65e0\u6cd5\u8fd0\u884c\u6216\u7248\u672c\u4e0d\u7b26\uff08\u8f93\u51fa\uff1a"{0}"\uff09' 'the unpacked uniflo.exe could not run or reported the wrong version (output: "{0}")') -f $reported) }

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
            Fail ((Pick '\u65e0\u6cd5\u5199\u5165 {0}\uff1a{1}' 'could not write {0}: {1}') -f $dest, $_.Exception.Message)
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

        Say ((Pick '\u5df2\u5b89\u88c5 {0} \u2192 {1}' 'installed {0} -> {1}') -f (& $dest --version | Out-String).Trim(), $dest)
        $onPath = ($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') }
        if (-not $onPath) {
            Say ((Pick '{0} \u4e0d\u5728 PATH \u4e2d\u3002\u5728 PowerShell \u91cc\u8fd0\u884c\u4e0b\u9762\u8fd9\u884c\u540e\u91cd\u5f00\u7ec8\u7aef\uff1a' '{0} is not on PATH. Run the following in PowerShell, then reopen the terminal:') -f $dir)
            Write-Host "    [Environment]::SetEnvironmentVariable('Path', '$dir;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        }

        if (-not $noSetup -and [Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected) {
            Say (Pick '\u8fd0\u884c uniflo setup\uff1a\u628a MCP / Skill \u63a5\u5165\u672c\u673a agent\uff08\u4e4b\u540e\u53ef\u7528 uniflo setup --uninstall \u64a4\u9500\uff09' 'running uniflo setup: wires MCP / Skill into local agents (undo any time with uniflo setup --uninstall)')
            & $dest setup
            if ($LASTEXITCODE -ne 0) { Say (Pick 'uniflo setup \u6ca1\u6709\u5b8c\u6210\uff0c\u4e4b\u540e\u53ef\u4ee5\u968f\u65f6\u8fd0\u884c uniflo setup' 'uniflo setup did not finish; you can run uniflo setup again any time') }
        } else {
            Say (Pick '\u8df3\u8fc7 uniflo setup\uff1b\u4e4b\u540e\u53ef\u8fd0\u884c uniflo setup \u628a MCP / Skill \u63a5\u5165\u672c\u673a agent' 'skipping uniflo setup; run uniflo setup later to wire MCP / Skill into local agents')
        }
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
} @args
