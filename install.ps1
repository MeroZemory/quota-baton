# Windows PowerShell 5.1 compatible; the leading param block also supports irm | iex.
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$Version,
    [Alias('LocalZipPath')][string]$ZipPath,
    [Alias('LocalChecksumPath')][string]$ChecksumPath,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\quota-baton'),
    [switch]$NoStart,
    [switch]$Yes,
    [switch]$Login,
    [ValidateSet('auto', 'ko', 'en')][string]$Lang = 'auto'
)

$ErrorActionPreference = 'Stop'
function Message([string]$ko, [string]$en) { if ($Lang -eq 'en') { return $en } else { return $ko } }
function Fail([string]$ko, [string]$en) { throw (Message $ko $en) }
function Say([string]$ko, [string]$en) { if ($Lang -eq 'en') { Write-Host $en } else { Write-Host $ko } }
function ConfigLanguage([string]$path) {
    # Only inspect [ui].language; do not render config or authentication contents.
    if (Test-Path -LiteralPath $path) {
        $section = ''
        foreach ($line in [IO.File]::ReadAllLines($path)) {
            if ($line -match '^\s*\[([^]]+)\]') { $section = $Matches[1] }
            if ($section -eq 'ui' -and $line -match '^\s*language\s*=\s*["''](en|ko)["'']') { return $Matches[1] }
        }
    }
    return 'ko'
}
function OwnedProcesses([string]$path) {
    @(Get-CimInstance Win32_Process -Filter "Name = 'quota-baton.exe' OR Name = 'quota-baton-tray.exe'" |
        Where-Object { $_.ExecutablePath -and [string]::Equals($_.ExecutablePath, $path, [StringComparison]::OrdinalIgnoreCase) })
}
function StopOwned($process, [string[]]$paths) {
    $current = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)"
    if ($current -and $current.CreationDate -eq $process.CreationDate -and $current.ExecutablePath -in $paths) {
        Stop-Process -Id $current.ProcessId -Force
        Get-Process -Id $current.ProcessId -ErrorAction SilentlyContinue | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (Get-CimInstance Win32_Process -Filter "ProcessId = $($current.ProcessId)" | Where-Object { $_.CreationDate -eq $current.CreationDate }) { Fail '프로세스가 종료되지 않았습니다.' 'Process did not stop.' }
    }
}
function RunCli([string[]]$Arguments) {
    & $exe @Arguments
    if ($LASTEXITCODE -ne 0) { Fail "quota-baton 명령 실패: $($Arguments -join ' ') (종료 코드 $LASTEXITCODE)" "quota-baton failed: $($Arguments -join ' ') (exit $LASTEXITCODE)" }
}
function SnapshotHealth([string]$json) {
    try {
        # qb-tray/src/client/snapshot.rs SnapshotBundleV1.health is the
        # qb-proto/src/lib.rs HealthResponse { ok: bool, version: String,
        # pid: u32, started_at: String }. There is no nested core.health field.
        $bundle = ConvertFrom-Json -InputObject $json
        $health = $bundle.health
        if ($bundle.schema_version -ne 1 -or $health.ok -isnot [bool] -or $health.version -isnot [string] -or
            ($health.pid -isnot [int] -and $health.pid -isnot [long]) -or $health.pid -le 0 -or $health.pid -gt [uint32]::MaxValue) { return $null }
        # PS 7 can decode ISO timestamps as DateTime; PS 5.1 leaves strings.
        if ($health.started_at -isnot [string] -and $health.started_at -isnot [datetime]) { return $null }
        if ($health.started_at -is [string]) { [void][DateTimeOffset]::Parse($health.started_at, [Globalization.CultureInfo]::InvariantCulture) }
        return $health
    } catch { return $null }
}
function ReadHealth {
    # CLI owns admin authentication and reads the configured port. Snapshot wraps
    # /admin/v1/health; the bootstrap never reads or handles tokens.
    $snapshotPath = Join-Path $scratch ('health-' + [guid]::NewGuid().ToString('N') + '.json')
    $probe = Start-Process -FilePath $exe -ArgumentList @('debug', 'snapshot', '--out', ('"' + $snapshotPath + '"')) -PassThru -WindowStyle Hidden
    if (-not $probe.WaitForExit(8000)) {
        $ownedProbe = @(OwnedProcesses $exe | Where-Object { $_.ProcessId -eq $probe.Id })
        foreach ($item in $ownedProbe) { StopOwned $item @($exe) }
        return $null
    }
    try {
        if ($probe.ExitCode -eq 0 -and (Test-Path -LiteralPath $snapshotPath)) {
            return (SnapshotHealth ([IO.File]::ReadAllText($snapshotPath)))
        }
    } finally { if (Test-Path -LiteralPath $snapshotPath) { [IO.File]::Delete($snapshotPath) } }
}
function WaitCoreReady($process, $previousStart) {
    $deadline = [DateTime]::UtcNow.AddSeconds(60)
    do {
        $health = ReadHealth
        $owned = @(OwnedProcesses $exe | Where-Object { $_.ProcessId -eq $process.Id })
        if ($health -and $health.ok -and $health.pid -eq $process.Id -and $owned.Count -eq 1 -and
            ($null -eq $previousStart -or $health.started_at -ne $previousStart)) { return $true }
        if ($process.HasExited) { return $false }
        Start-Sleep -Milliseconds 300
    } while ([DateTime]::UtcNow -lt $deadline)
    return $false
}
function EnsureCoreReady {
    $running = @(OwnedProcesses $exe)
    if ($running.Count) {
        $health = ReadHealth
        if ($health -and $health.ok -and $health.pid -in $running.ProcessId) { return }
        Fail '설치된 코어의 준비 상태를 확인하지 못했습니다.' 'Installed core readiness validation failed.'
    }
    $started = Start-Process -FilePath $exe -ArgumentList @('serve') -PassThru -WindowStyle Hidden
    if (-not (WaitCoreReady $started $null)) { Fail '새 코어의 준비 상태를 확인하지 못했습니다. wire를 적용하지 않았습니다.' 'Fresh core readiness failed; wire was not applied.' }
}
function CompleteSetup {
    $hasClients = $detected.codex -or $detected.claude
    if ($hasClients) {
        $codexMode = if ($detected.codex) { 'dedicated' } else { 'none' }
        $claudeMode = if ($detected.claude) { 'dedicated' } else { 'none' }
        $wire = @('wire', '--codex-mode', $codexMode, '--claude-mode', $claudeMode)
        RunCli $wire
        if ($NoStart) {
            Say 'NoStart: wire 미리보기만 표시했습니다. 적용과 로그인을 건너뜁니다. 코어 시작 후 wire --apply를 실행해 주세요.' 'NoStart: preview only; skipping wire apply and login. Start the core before running wire --apply.'
        } else {
            $apply = $Yes
            if (-not $apply) {
                $prompt = if ($Lang -eq 'en') { 'Apply? [Y/n]' } else { '적용할까요? [Y/n]' }
                $answer = Read-Host $prompt
                $apply = [string]::IsNullOrWhiteSpace($answer) -or $answer -match '^(?i)y(es)?$'
            }
            if ($apply) {
                # wire --apply requires a healthy authenticated admin endpoint,
                # even on a fresh install: initialize the core BEFORE applying.
                EnsureCoreReady
                RunCli ($wire + '--apply')
            }
        }
    } else {
        Say '클라이언트를 찾지 못해 wire를 건너뜁니다. Codex 또는 Claude 설치 후 wire를 실행해 주세요.' 'No clients detected; skipping wire. Install Codex or Claude, then run wire.'
        if ($NoStart -and $Login) { Say 'NoStart: 로그인을 건너뜁니다.' 'NoStart: skipping login.' }
    }
    if (-not $NoStart -and @(OwnedProcesses $tray).Count -eq 0) { Start-Process -FilePath $tray -WindowStyle Hidden | Out-Null }
    $accounts = @(RunCli @('accounts', 'list'))
    $accounts | ForEach-Object { Write-Host $_ }
    foreach ($provider in @('codex', 'claude')) {
        # accounts_cli.rs prints id, provider, email, ... as TSV in this order.
        $hasAccount = @($accounts | Where-Object { $cells = $_ -split "`t"; $cells.Count -ge 3 -and $cells[1] -eq $provider }).Count -gt 0
        if ($detected[$provider] -and -not $hasAccount) {
            Say '계정 로그인 명령:' 'Account login command:'
            Write-Host ('& "' + $exe + '" accounts login ' + $provider)
            if ($Login -and -not $NoStart) { EnsureCoreReady; RunCli @('accounts', 'login', $provider) }
        }
    }
    RunCli @('status')
}

$dataDir = if ($env:QB_HOME) { $env:QB_HOME } else { Join-Path $env:LOCALAPPDATA 'quota-baton' }
if ($Lang -eq 'auto') { $Lang = ConfigLanguage (Join-Path $dataDir 'config.toml') }
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\', '/')
if ($InstallDir -eq [IO.Path]::GetPathRoot($InstallDir).TrimEnd('\') -or $InstallDir.Contains('"')) { Fail '설치 경로를 확인해 주세요.' 'Unsafe InstallDir.' }
$exe = Join-Path $InstallDir 'quota-baton.exe'
$tray = Join-Path $InstallDir 'quota-baton-tray.exe'
$scratch = $null
try {
    if (-not $ZipPath) {
        if ($ChecksumPath) { Fail 'ChecksumPath에는 ZipPath가 필요합니다.' 'ChecksumPath requires ZipPath.' }
        $tag = if ($Version) { 'tags/v' + $Version.TrimStart('v') } else { 'latest' }
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/MeroZemory/quota-baton/releases/$tag" -Headers @{ 'User-Agent' = 'quota-baton-installer' }
        # Public v0.5.0 asset: quota-baton-0.5.0-windows-x64.zip;
        # require its exact name plus .sha256, allowing Rust-target spellings too.
        $assets = @($release.assets | Where-Object { $_.name -match '(?i)(windows|win|pc-windows).*(x64|x86_64|amd64).*\.zip$|(?i)(x64|x86_64|amd64).*windows.*\.zip$' })
        if ($assets.Count -ne 1) { Fail 'Windows x64 릴리스 zip이 정확히 하나 있어야 합니다.' 'Expected exactly one Windows x64 release zip.' }
        $asset = $assets[0]
        $checksums = @($release.assets | Where-Object { $_.name -eq ($asset.name + '.sha256') })
        if ($checksums.Count -ne 1) { Fail '릴리스 zip.sha256 파일이 없습니다.' 'Release zip.sha256 asset missing.' }
        $scratch = Join-Path ([IO.Path]::GetTempPath()) ('quota-baton-install-' + [guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($scratch)
        $ZipPath = Join-Path $scratch 'release.zip'
        $ChecksumPath = Join-Path $scratch 'release.zip.sha256'
        Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $ZipPath
        Invoke-WebRequest -UseBasicParsing -Uri $checksums[0].browser_download_url -OutFile $ChecksumPath
    } elseif ($Version) { Fail 'Version과 ZipPath 중 하나만 지정해 주세요.' 'Use Version or ZipPath, not both.' }
    if (-not $ChecksumPath) { $ChecksumPath = $ZipPath + '.sha256' }
    $checksum = [IO.File]::ReadAllText([IO.Path]::GetFullPath($ChecksumPath)).Trim()
    if ($checksum -notmatch '\A([a-fA-F0-9]{64})(?:[ \t]+\*?[^\r\n]+)?\z') { Fail 'SHA256 파일 형식이 올바르지 않습니다.' 'Invalid SHA256 sidecar.' }
    $expected = $Matches[1]
    if ((Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash -ne $expected) { Fail 'SHA256 체크섬이 일치하지 않습니다.' 'SHA256 checksum mismatch.' }
    Say 'SHA256 검증 완료.' 'SHA256 verified.'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead([IO.Path]::GetFullPath($ZipPath))
    try {
        # Select the two fixed payload names; never execute a downloaded script,
        # or extract arbitrary archive paths (including traversal or reparse links).
        $payload = @{}
        foreach ($name in @('quota-baton.exe', 'quota-baton-tray.exe')) {
            $entries = @($archive.Entries | Where-Object { $_.Name -eq $name })
            if ($entries.Count -ne 1 -or $entries[0].Length -eq 0) { Fail "패키지 실행 파일이 없거나 중복됩니다: $name" "Missing or ambiguous payload: $name" }
            $payload[$name] = $entries[0]
        }
        $detected = @{}
        foreach ($provider in @('codex', 'claude')) {
            $command = Get-Command $provider -ErrorAction SilentlyContinue | Select-Object -First 1
            $native = Join-Path $env:USERPROFILE '.local\bin\claude.exe'
            $found = if ($command -and -not [string]::IsNullOrWhiteSpace($command.Source)) { $command.Source } elseif ($provider -eq 'claude' -and (Test-Path -LiteralPath $native)) { $native } else { $null }
            $detected[$provider] = [bool]$found
            if ($found) { Say "$provider 발견: $found" "$provider detected: $found" } else { Say "$provider 없음" "$provider not found" }
        }
        if (-not $PSCmdlet.ShouldProcess($InstallDir, (Message '검증한 quota-baton 설치, 사용자 PATH 추가 및 wire 미리보기' 'Install verified quota-baton, update user PATH and preview wiring'))) {
            Say '미리보기: 실행 파일·PATH·wire·프로세스·설정은 변경하지 않았습니다.' 'Dry run: no executable, PATH, wire, process or configuration changes.'
            return
        }
        $oldCore = @(OwnedProcesses $exe)
        $oldTray = @(OwnedProcesses $tray)
        if ($NoStart -and ($oldCore.Count -or $oldTray.Count)) { Fail 'NoStart: 설치된 프로세스를 종료한 뒤 다시 실행해 주세요.' 'NoStart: installed processes are running; close them before replacing binaries.' }
        if (-not $scratch) {
            $scratch = Join-Path ([IO.Path]::GetTempPath()) ('quota-baton-install-' + [guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($scratch)
        }
        $oldHealth = if ($oldCore.Count) { ReadHealth } else { $null }
        if ($oldCore.Count -and (-not $oldHealth -or -not $oldHealth.ok -or $oldHealth.pid -notin $oldCore.ProcessId)) { Fail 'takeover 전 기존 코어 상태를 확인하지 못했습니다.' 'Cannot validate the installed core before takeover.' }
        [void][IO.Directory]::CreateDirectory($InstallDir)
        # Refuse reparse destinations rather than writing through another location.
        $ancestor = Get-Item -LiteralPath $InstallDir
        while ($ancestor) {
            if ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail '설치 경로에 재분석 지점이 있습니다.' 'InstallDir must not traverse a reparse point.' }
            $ancestor = $ancestor.Parent
        }
        $stamp = [DateTime]::UtcNow.ToString('yyyyMMddHHmmssfffffff')
        $renamed = @{}
        $installed = @()
        try {
            foreach ($name in @('quota-baton.exe', 'quota-baton-tray.exe')) {
                $destination = Join-Path $InstallDir $name
                $staged = Join-Path $scratch $name
                [IO.Compression.ZipFileExtensions]::ExtractToFile($payload[$name], $staged, $false)
                if (Test-Path -LiteralPath $destination) {
                    if ((Get-Item -LiteralPath $destination).Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail '실행 파일이 재분석 지점입니다.' 'Executable is a reparse point.' }
                    $backup = $destination + '.old-' + $stamp
                    [IO.File]::Move($destination, $backup)
                    $renamed[$destination] = $backup
                }
                [IO.File]::Copy($staged, $destination)
                $installed += $destination
            }
        } catch {
            foreach ($destination in $installed) { [IO.File]::Delete($destination) }
            foreach ($destination in $renamed.Keys) { [IO.File]::Move($renamed[$destination], $destination) }
            throw
        }
    } finally { $archive.Dispose() }

    if ($oldCore.Count) {
        $successor = Start-Process -FilePath $exe -ArgumentList @('serve', '--takeover') -PassThru -WindowStyle Hidden
        $healthy = WaitCoreReady $successor $oldHealth.started_at
        if (-not $healthy) { Fail 'takeover 상태 검증 실패. 롤백 파일을 보존했습니다. 재시도 전 status를 확인해 주세요.' 'Takeover health validation failed; rollback binaries retained. Inspect status before retrying.' }
    }
    if (-not $NoStart) {
        foreach ($process in $oldTray) { StopOwned $process @($tray, ($tray + '.old-' + $stamp)) }
    }
    # A generation is a rollback pair: never prune just one half of a pair.
    $backups = @(Get-ChildItem -LiteralPath $InstallDir -File | Where-Object { $_.Name -match '^quota-baton(?:-tray)?\.exe\.old-\d{21}$' })
    $generations = @($backups | ForEach-Object { ($_.Name -split '\.old-')[1] } | Sort-Object -Descending -Unique)
    foreach ($backup in $backups) {
        if ($generations.Count -gt 0 -and -not $backup.Name.EndsWith($generations[0])) { [IO.File]::Delete($backup.FullName) }
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $present = @($userPath -split ';' | Where-Object { [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\', '/') -ieq $InstallDir }).Count -gt 0
    if (-not $present) {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { $userPath.TrimEnd(';') + ';' + $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Say '사용자 PATH 추가 완료. 새 터미널에서 적용됩니다.' 'Added user PATH. Open a new terminal to pick it up.'
    }
    CompleteSetup
} finally {
    if ($scratch -and [IO.Directory]::Exists($scratch)) { [IO.Directory]::Delete($scratch, $true) }
}
