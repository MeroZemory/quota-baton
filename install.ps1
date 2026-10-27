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
function Korean([string]$key) {
    $text = @{
        m1 = '7IKs7Jqp'
        m2 = '7KSR'
        m3 = '64uk7J2M'
        m4 = '7Iuk7ZaJ'
        m5 = '65WM'
        m6 = '7KCc6rGw'
        m7 = '66Gk67CxIO2MjOydvCDsoJXrpqzrpbwg6rG064SI65uw7JeI7Iq164uI64ukLiDri6TsnYwg7Iuk7ZaJIOuVjCDri6Tsi5wg7Iuc64+E7ZWp64uI64ukLg=='
        m8 = '7ZSE66Gc7IS47Iqk6rCAIOyiheujjOuQmOyngCDslYrslZjsirXri4jri6Qu'
        m9 = '66qF66C5'
        m10 = '7Iuk7Yyo'
        m11 = '7KKF66OM'
        m12 = '7L2U65Oc'
        m13 = '7IOB7YOcIO2ZleyduCDsnoTsi5wg7YyM7J28IOygleumrOulvCDqsbTrhIjrm7Dsl4jsirXri4jri6Qu'
        m14 = '7ISk7LmY65CcIOy9lOyWtOydmCDspIDruYQg7IOB7YOc66W8IO2ZleyduO2VmOyngCDrqrvtlojsirXri4jri6Qu'
        m15 = '7IOIIOy9lOyWtOydmCDspIDruYQg7IOB7YOc66W8IO2ZleyduO2VmOyngCDrqrvtlojsirXri4jri6QuIHdpcmXrpbwg7KCB7Jqp7ZWY7KeAIOyViuyVmOyKteuLiOuLpC4='
        m16 = 'Tm9TdGFydDogd2lyZSDrr7jrpqzrs7TquLDrp4wg7ZGc7Iuc7ZaI7Iq164uI64ukLiDsoIHsmqnqs7wg66Gc6re47J247J2EIOqxtOuEiOucgeuLiOuLpC4g7L2U7Ja0IOyLnOyekSDtm4Qgd2lyZSAtLWFwcGx566W8IOyLpO2Wie2VtCDso7zshLjsmpQu'
        m17 = '7KCB7Jqp7ZWg6rmM7JqUPyBbWS9uXQ=='
        m18 = '7YG065287J207Ja47Yq466W8IOywvuyngCDrqrvtlbQgd2lyZeulvCDqsbTrhIjrnIHri4jri6QuIENvZGV4IOuYkOuKlCBDbGF1ZGUg7ISk7LmYIO2bhCB3aXJl66W8IOyLpO2Wie2VtCDso7zshLjsmpQu'
        m19 = 'Tm9TdGFydDog66Gc6re47J247J2EIOqxtOuEiOucgeuLiOuLpC4='
        m20 = '6rOE7KCVIOuhnOq3uOyduCDrqoXroLk6'
        m21 = '7ISk7LmYIOqyveuhnOulvCDtmZXsnbjtlbQg7KO87IS47JqULg=='
        m22 = 'Q2hlY2tzdW1QYXRo7JeQ64qUIFppcFBhdGjqsIAg7ZWE7JqU7ZWp64uI64ukLg=='
        m23 = 'V2luZG93cyB4NjQg66a066as7IqkIHppcOydtCDsoJXtmZXtnogg7ZWY64KYIOyeiOyWtOyVvCDtlanri4jri6Qu'
        m24 = '66a066as7IqkIHppcC5zaGEyNTYg7YyM7J287J20IOyXhuyKteuLiOuLpC4='
        m25 = 'VmVyc2lvbuqzvCBaaXBQYXRoIOykkSDtlZjrgpjrp4wg7KeA7KCV7ZW0IOyjvOyEuOyalC4='
        m26 = 'U0hBMjU2IO2MjOydvCDtmJXsi53snbQg7Jis67CU66W07KeAIOyViuyKteuLiOuLpC4='
        m27 = 'U0hBMjU2IOyytO2BrOyErOydtCDsnbzsuZjtlZjsp4Ag7JWK7Iq164uI64ukLg=='
        m28 = 'U0hBMjU2IOqygOymnSDsmYTro4wu'
        m29 = '7Yyo7YKk7KeA'
        m30 = '7YyM7J287J20'
        m31 = '7JeG6rGw64KY'
        m32 = '7KSR67O165Cp64uI64uk'
        m33 = '67Cc6rKs'
        m34 = '656Y7Y28'
        m35 = '6rK97Jyg'
        m36 = '7JeG7J2M'
        m37 = '6rKA7Kad7ZWcIHF1b3RhLWJhdG9uIOyEpOy5mCwg7IKs7Jqp7J6QIFBBVEgg7LaU6rCAIOuwjyB3aXJlIOuvuOumrOuztOq4sA=='
        m38 = '66+466as67O06riwOiDsi6Ttlokg7YyM7J28wrdQQVRIwrd3aXJlwrftlITroZzshLjsiqTCt+yEpOygleydgCDrs4Dqsr3tlZjsp4Ag7JWK7JWY7Iq164uI64ukLg=='
        m39 = 'Tm9TdGFydDog7ISk7LmY65CcIO2UhOuhnOyEuOyKpOulvCDsooXro4ztlZwg65KkIOuLpOyLnCDsi6TtlontlbQg7KO87IS47JqULg=='
        m40 = 'dGFrZW92ZXIg7KCEIOq4sOyhtCDsvZTslrQg7IOB7YOc66W8IO2ZleyduO2VmOyngCDrqrvtlojsirXri4jri6Qu'
        m41 = '7ISk7LmYIOqyveuhnOyXkCDsnqzrtoTshJ0g7KeA7KCQ7J20IOyeiOyKteuLiOuLpC4='
        m42 = '7Iuk7ZaJIO2MjOydvOydtCDsnqzrtoTshJ0g7KeA7KCQ7J6F64uI64ukLg=='
        m43 = 'dGFrZW92ZXIg7IOB7YOcIOqygOymnSDsi6TtjKguIOuhpOuwsSDtjIzsnbzsnYQg67O07KG07ZaI7Iq164uI64ukLiDsnqzsi5zrj4Qg7KCEIHN0YXR1c+ulvCDtmZXsnbjtlbQg7KO87IS47JqULg=='
        m44 = '7IKs7Jqp7J6QIFBBVEgg7LaU6rCAIOyZhOujjC4g7IOIIO2EsOuvuOuEkOyXkOyEnCDsoIHsmqnrkKnri4jri6Qu'
        m45 = '7ISk7LmYIOyZhOujjA=='
        m46 = '7ISk7LmYIOuvuOyZhOujjA=='
        m47 = '7Yq466CI7J20'
        m48 = '7ZmV7J24'
        m49 = '7Iuk7ZaJ7J2E'
        m50 = '7ZmV7J247ZWY7KeA'
        m51 = '66q77ZaI7Iq164uI64uk'
        m52 = '7IiY64+Z7Jy866Gc'
        m53 = '7Iuc7J6R7ZW0'
        m54 = '7KO87IS47JqU'
        m55 = '7ISk7LmYIOyehOyLnCDtj7TrjZQg7KCV66as66W8IOqxtOuEiOubsOyXiOyKteuLiOuLpC4='
        m56 = '67aA67aEIOyEpOy5mCDtjIzsnbzsnYQg7KCc6rGw7ZWY7KeAIOuqu+2WiOyKteuLiOuLpA=='
        m57 = '66Gk67CxIO2MjOydvOydhCDrs7TsobTtlojsirXri4jri6Q='
        m58 = '7Yq466CI7J20IOuzteq1rOyXkCDsi6TtjKjtlojsirXri4jri6QuIOyEpOy5mOqwgCDrr7jsmYTro4zsnbwg7IiYIOyeiOyKteuLiOuLpC4g7Yq466CI7J2066W8IOyImOuPmeycvOuhnCDsi5zsnpHtlbQg7KO87IS47JqULg=='
    }
    return [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($text[$key]))
}
function Message([string]$ko, [string]$en) { if ($Lang -eq 'en') { return $en } else { return $ko } }
function Fail([string]$ko, [string]$en) { throw (Message $ko $en) }
function Say([string]$ko, [string]$en) { if ($Lang -eq 'en') { Write-Host $en } else { Write-Host $ko } }
function SafeClientFile([string]$path) {
    if ([string]::IsNullOrWhiteSpace($path) -or -not [IO.Path]::IsPathRooted($path) -or [IO.Path]::GetPathRoot($path).Length -lt 3 -or
        $path -match '[\r\n";%!^&|<>]' -or $path -match '(^|[\\/])\.\.([\\/]|$)') { return $null }
    try {
        $item = Get-Item -LiteralPath $path -ErrorAction Stop
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $null }
        # Rust canonicalizes ancestors and rechecks command safety. Conservatively
        # refuse aliases through junctions rather than report a different target.
        $ancestor = $item.Directory
        while ($ancestor) {
            if ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $null }
            $ancestor = $ancestor.Parent
        }
        return $item.FullName
    } catch { }
    return $null
}
function ClientPathDirectories([string]$path) {
    # Windows PATH separators apply only outside quotes; quotes are not path bytes.
    $directory = ''
    $quoted = $false
    foreach ($character in $path.ToCharArray()) {
        if ($character -eq '"') { $quoted = -not $quoted }
        elseif ($character -eq ';' -and -not $quoted) { $directory; $directory = '' }
        else { $directory += $character }
    }
    $directory
}
function ResolveClient([string]$provider) {
    # Read paths only. Never inspect or execute a wrapper body.
    $herdrBin = [IO.Path]::GetFullPath((Join-Path $env:APPDATA 'herdr\bin')).TrimEnd('\', '/')
    $command = Get-Command $provider -ErrorAction SilentlyContinue | Select-Object -First 1
    $wrapper = $null
    if ($command -and $command.Source -and [IO.Path]::IsPathRooted($command.Source) -and
        [IO.Path]::GetDirectoryName($command.Source) -and
        [string]::Equals([IO.Path]::GetDirectoryName($command.Source).TrimEnd('\', '/'), $herdrBin, [StringComparison]::OrdinalIgnoreCase)) { $wrapper = $command.Source }
    if ($provider -eq 'codex') {
        return [pscustomobject]@{ Path = (SafeClientFile (Join-Path $env:APPDATA 'npm\codex.cmd')); WrapperPath = $wrapper }
    }
    $native = SafeClientFile (Join-Path $env:USERPROFILE '.local\bin\claude.exe')
    if ($native) { return [pscustomobject]@{ Path = $native; WrapperPath = $wrapper } }
    $extensions = if ($null -eq $env:PATHEXT) { '.COM;.EXE;.BAT;.CMD' } else { $env:PATHEXT }
    foreach ($directory in (ClientPathDirectories $env:PATH)) {
        if ([string]::IsNullOrWhiteSpace($directory) -or $directory.Contains(';')) { continue }
        if (-not [IO.Path]::IsPathRooted($directory) -or [IO.Path]::GetPathRoot($directory).Length -lt 3) { continue }
        try {
            if ([string]::Equals([IO.Path]::GetFullPath($directory).TrimEnd('\', '/'), $herdrBin, [StringComparison]::OrdinalIgnoreCase)) { continue }
        } catch { continue }
        foreach ($extension in ($extensions -split ';')) {
            if ($extension -notmatch '^\.(COM|EXE|BAT|CMD)$') { continue }
            $found = SafeClientFile (Join-Path $directory ('claude' + $extension))
            if ($found) { return [pscustomobject]@{ Path = $found; WrapperPath = $wrapper } }
        }
    }
    return [pscustomobject]@{ Path = $null; WrapperPath = $wrapper }
}
function RetainRollbackGenerations([string]$directory) {
    try {
        $backups = @(Get-ChildItem -LiteralPath $directory -File -ErrorAction Stop | Where-Object { $_.Name -match '^quota-baton(?:-tray)?\.exe\.old-\d{21}$' })
        $generations = @($backups | ForEach-Object { ($_.Name -split '\.old-')[1] } | Sort-Object -Descending -Unique -ErrorAction Stop)
        foreach ($backup in $backups) {
            if ($generations.Count -gt 0 -and -not $backup.Name.EndsWith($generations[0])) {
                try { [IO.File]::Delete($backup.FullName) }
                catch { Say "$(Korean 'm1') $(Korean 'm2'), $(Korean 'm3') $(Korean 'm4') $(Korean 'm5') $(Korean 'm6'): $($backup.FullName)" "In use, will be removed next time: $($backup.FullName)" }
            }
        }
    } catch { Say (Korean 'm7') 'Rollback cleanup skipped; will retry next time.' }
}
function RestoreInstallerTray([string]$currentTray, [string]$fallbackTray) {
    $paths = @($currentTray, $fallbackTray) | Where-Object { $_ } | Select-Object -Unique
    # A partially stopped old tray may still run from its renamed image.
    foreach ($path in $paths) {
        try { if (@(OwnedProcesses $path).Count) { return $path } } catch { }
    }
    foreach ($path in $paths) {
        try {
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { continue }
            $started = Start-Process -FilePath $path -PassThru -WindowStyle Hidden
            if (-not $started.WaitForExit(1000) -and -not $started.HasExited -and
                @(OwnedProcesses $path | Where-Object { $_.ProcessId -eq $started.Id }).Count -eq 1) { return $path }
        } catch { }
    }
    return $null
}
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
    @(Get-CimInstance Win32_Process -Filter "Name LIKE 'quota-baton%'" |
        Where-Object { $_.ExecutablePath -and [string]::Equals($_.ExecutablePath, $path, [StringComparison]::OrdinalIgnoreCase) })
}
function StopOwned($process, [string[]]$paths) {
    $current = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)"
    if ($current -and $current.CreationDate -eq $process.CreationDate -and $current.ExecutablePath -in $paths) {
        Stop-Process -Id $current.ProcessId -Force
        Get-Process -Id $current.ProcessId -ErrorAction SilentlyContinue | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
        if (Get-CimInstance Win32_Process -Filter "ProcessId = $($current.ProcessId)" | Where-Object { $_.CreationDate -eq $current.CreationDate }) { Fail (Korean 'm8') 'Process did not stop.' }
    }
}
function RunCli([string[]]$Arguments) {
    $wireCall = $Arguments.Count -gt 0 -and $Arguments[0] -eq 'wire' -and $resolvedClients -and $resolvedClients.claude.Path
    $savedPath = $env:PATH
    $savedPathExt = $env:PATHEXT
    $hadPath = Test-Path Env:PATH
    $hadPathExt = Test-Path Env:PATHEXT
    try {
        if ($wireCall) {
            # Process-local only: wire resolves the exact displayed real client.
            $clientPath = $resolvedClients.claude.Path
            $env:PATH = [IO.Path]::GetDirectoryName($clientPath) + ';' + $savedPath
            $env:PATHEXT = [IO.Path]::GetExtension($clientPath) + ';' + $savedPathExt
        }
        & $exe @Arguments
        if ($LASTEXITCODE -ne 0) { Fail "quota-baton $(Korean 'm9') $(Korean 'm10'): $($Arguments -join ' ') ($(Korean 'm11') $(Korean 'm12') $LASTEXITCODE)" "quota-baton failed: $($Arguments -join ' ') (exit $LASTEXITCODE)" }
    } finally {
        if ($wireCall) {
            if ($hadPath) { $env:PATH = $savedPath } else { Remove-Item Env:PATH -ErrorAction SilentlyContinue }
            if ($hadPathExt) { $env:PATHEXT = $savedPathExt } else { Remove-Item Env:PATHEXT -ErrorAction SilentlyContinue }
        }
    }
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
    try {
        $probe = Start-Process -FilePath $exe -ArgumentList @('debug', 'snapshot', '--out', ('"' + $snapshotPath + '"')) -PassThru -WindowStyle Hidden
        if (-not $probe.WaitForExit(8000)) {
            $ownedProbe = @(OwnedProcesses $exe | Where-Object { $_.ProcessId -eq $probe.Id })
            foreach ($item in $ownedProbe) { StopOwned $item @($exe) }
            return $null
        }
        if ($probe.ExitCode -eq 0 -and (Test-Path -LiteralPath $snapshotPath)) {
            return (SnapshotHealth ([IO.File]::ReadAllText($snapshotPath)))
        }
    } finally {
        try { if (Test-Path -LiteralPath $snapshotPath) { [IO.File]::Delete($snapshotPath) } }
        catch { Write-Warning (Message (Korean 'm13') 'Snapshot cleanup skipped.') -WarningAction Continue }
    }
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
        Fail (Korean 'm14') 'Installed core readiness validation failed.'
    }
    $started = Start-Process -FilePath $exe -ArgumentList @('serve') -PassThru -WindowStyle Hidden
    if (-not (WaitCoreReady $started $null)) { Fail (Korean 'm15') 'Fresh core readiness failed; wire was not applied.' }
}
function CompleteSetup {
    $hasClients = $detected.codex -or $detected.claude
    if ($hasClients) {
        $codexMode = if ($detected.codex) { 'dedicated' } else { 'none' }
        $claudeMode = if ($detected.claude) { 'dedicated' } else { 'none' }
        $wire = @('wire', '--codex-mode', $codexMode, '--claude-mode', $claudeMode)
        RunCli $wire
        if ($NoStart) {
            Say (Korean 'm16') 'NoStart: preview only; skipping wire apply and login. Start the core before running wire --apply.'
        } else {
            $apply = $Yes
            if (-not $apply) {
                $prompt = if ($Lang -eq 'en') { 'Apply? [Y/n]' } else { (Korean 'm17') }
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
        Say (Korean 'm18') 'No clients detected; skipping wire. Install Codex or Claude, then run wire.'
        if ($NoStart -and $Login) { Say (Korean 'm19') 'NoStart: skipping login.' }
    }
    if (-not $NoStart -and @(OwnedProcesses $tray).Count -eq 0) { Start-Process -FilePath $tray -WindowStyle Hidden | Out-Null }
    $accounts = @(RunCli @('accounts', 'list'))
    $accounts | ForEach-Object { Write-Host $_ }
    foreach ($provider in @('codex', 'claude')) {
        # accounts_cli.rs prints id, provider, email, ... as TSV in this order.
        $hasAccount = @($accounts | Where-Object { $cells = $_ -split "`t"; $cells.Count -ge 3 -and $cells[1] -eq $provider }).Count -gt 0
        if ($detected[$provider] -and -not $hasAccount) {
            Say (Korean 'm20') 'Account login command:'
            Write-Host ('& "' + $exe + '" accounts login ' + $provider)
            if ($Login -and -not $NoStart) { EnsureCoreReady; RunCli @('accounts', 'login', $provider) }
        }
    }
    RunCli @('status')
}

$dataDir = if ($env:QB_HOME) { $env:QB_HOME } else { Join-Path $env:LOCALAPPDATA 'quota-baton' }
if ($Lang -eq 'auto') { $Lang = ConfigLanguage (Join-Path $dataDir 'config.toml') }
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\', '/')
if ($InstallDir -eq [IO.Path]::GetPathRoot($InstallDir).TrimEnd('\') -or $InstallDir.Contains('"')) { Fail (Korean 'm21') 'Unsafe InstallDir.' }
$exe = Join-Path $InstallDir 'quota-baton.exe'
$tray = Join-Path $InstallDir 'quota-baton-tray.exe'
$scratch = $null
$trayRepairRequired = $false
$retentionRequired = $false
$fallbackTray = $null
$setupComplete = $false
try {
    if (-not $ZipPath) {
        if ($ChecksumPath) { Fail (Korean 'm22') 'ChecksumPath requires ZipPath.' }
        $tag = if ($Version) { 'tags/v' + $Version.TrimStart('v') } else { 'latest' }
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/MeroZemory/quota-baton/releases/$tag" -Headers @{ 'User-Agent' = 'quota-baton-installer' }
        # Public v0.5.0 asset: quota-baton-0.5.0-windows-x64.zip;
        # require its exact name plus .sha256, allowing Rust-target spellings too.
        $assets = @($release.assets | Where-Object { $_.name -match '(?i)(windows|win|pc-windows).*(x64|x86_64|amd64).*\.zip$|(?i)(x64|x86_64|amd64).*windows.*\.zip$' })
        if ($assets.Count -ne 1) { Fail (Korean 'm23') 'Expected exactly one Windows x64 release zip.' }
        $asset = $assets[0]
        $checksums = @($release.assets | Where-Object { $_.name -eq ($asset.name + '.sha256') })
        if ($checksums.Count -ne 1) { Fail (Korean 'm24') 'Release zip.sha256 asset missing.' }
        $scratch = Join-Path ([IO.Path]::GetTempPath()) ('quota-baton-install-' + [guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($scratch)
        $ZipPath = Join-Path $scratch 'release.zip'
        $ChecksumPath = Join-Path $scratch 'release.zip.sha256'
        Invoke-WebRequest -UseBasicParsing -Uri $asset.browser_download_url -OutFile $ZipPath
        Invoke-WebRequest -UseBasicParsing -Uri $checksums[0].browser_download_url -OutFile $ChecksumPath
    } elseif ($Version) { Fail (Korean 'm25') 'Use Version or ZipPath, not both.' }
    if (-not $ChecksumPath) { $ChecksumPath = $ZipPath + '.sha256' }
    $checksum = [IO.File]::ReadAllText([IO.Path]::GetFullPath($ChecksumPath)).Trim()
    if ($checksum -notmatch '\A([a-fA-F0-9]{64})(?:[ \t]+\*?[^\r\n]+)?\z') { Fail (Korean 'm26') 'Invalid SHA256 sidecar.' }
    $expected = $Matches[1]
    if ((Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash -ne $expected) { Fail (Korean 'm27') 'SHA256 checksum mismatch.' }
    Say (Korean 'm28') 'SHA256 verified.'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead([IO.Path]::GetFullPath($ZipPath))
    try {
        # Select the two fixed payload names; never execute a downloaded script,
        # or extract arbitrary archive paths (including traversal or reparse links).
        $payload = @{}
        foreach ($name in @('quota-baton.exe', 'quota-baton-tray.exe')) {
            $entries = @($archive.Entries | Where-Object { $_.Name -eq $name })
            if ($entries.Count -ne 1 -or $entries[0].Length -eq 0) { Fail "$(Korean 'm29') $(Korean 'm4') $(Korean 'm30') $(Korean 'm31') $(Korean 'm32'): $name" "Missing or ambiguous payload: $name" }
            $payload[$name] = $entries[0]
        }
        $detected = @{}
        $resolvedClients = @{}
        foreach ($provider in @('codex', 'claude')) {
            $client = ResolveClient $provider
            $resolvedClients[$provider] = $client
            $found = $client.Path
            $detected[$provider] = [bool]$found
            if ($found) {
                if ($client.WrapperPath) { Say "$provider $(Korean 'm33'): $found (herdr $(Korean 'm34') $(Korean 'm35'): $($client.WrapperPath))" "$provider detected: $found (via herdr wrapper: $($client.WrapperPath))" }
                else { Say "$provider $(Korean 'm33'): $found" "$provider detected: $found" }
            } else { Say "$provider $(Korean 'm36')" "$provider not found" }
        }
        if (-not $PSCmdlet.ShouldProcess($InstallDir, (Message (Korean 'm37') 'Install verified quota-baton, update user PATH and preview wiring'))) {
            Say (Korean 'm38') 'Dry run: no executable, PATH, wire, process or configuration changes.'
            return
        }
        $oldCore = @(OwnedProcesses $exe)
        $oldTray = @(OwnedProcesses $tray)
        if ($NoStart -and ($oldCore.Count -or $oldTray.Count)) { Fail (Korean 'm39') 'NoStart: installed processes are running; close them before replacing binaries.' }
        if (-not $scratch) {
            $scratch = Join-Path ([IO.Path]::GetTempPath()) ('quota-baton-install-' + [guid]::NewGuid().ToString('N'))
            [void][IO.Directory]::CreateDirectory($scratch)
        }
        $oldHealth = if ($oldCore.Count) { ReadHealth } else { $null }
        if ($oldCore.Count -and (-not $oldHealth -or -not $oldHealth.ok -or $oldHealth.pid -notin $oldCore.ProcessId)) { Fail (Korean 'm40') 'Cannot validate the installed core before takeover.' }
        [void][IO.Directory]::CreateDirectory($InstallDir)
        # Refuse reparse destinations rather than writing through another location.
        $ancestor = Get-Item -LiteralPath $InstallDir
        while ($ancestor) {
            if ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail (Korean 'm41') 'InstallDir must not traverse a reparse point.' }
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
                    if ((Get-Item -LiteralPath $destination).Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail (Korean 'm42') 'Executable is a reparse point.' }
                    $backup = $destination + '.old-' + $stamp
                    [IO.File]::Move($destination, $backup)
                    $renamed[$destination] = $backup
                }
                # Track before Copy: failure may leave a partial destination.
                $installed += $destination
                [IO.File]::Copy($staged, $destination)
            }
        } catch {
            foreach ($destination in $installed) {
                try { if (Test-Path -LiteralPath $destination) { [IO.File]::Delete($destination) } }
                catch { Write-Warning (Message "$(Korean 'm56'): $destination" "Rollback could not remove partial payload: $destination") -WarningAction Continue }
            }
            foreach ($destination in $renamed.Keys) {
                try { [IO.File]::Move($renamed[$destination], $destination) }
                catch { Write-Warning (Message "$(Korean 'm57'): $($renamed[$destination])" "Rollback binary retained: $($renamed[$destination])") -WarningAction Continue }
            }
            throw
        }
    } finally { $archive.Dispose() }

    $fallbackTray = $renamed[$tray]
    $retentionRequired = $true
    if ($oldCore.Count) {
        $successor = Start-Process -FilePath $exe -ArgumentList @('serve', '--takeover') -PassThru -WindowStyle Hidden
        $healthy = WaitCoreReady $successor $oldHealth.started_at
        if (-not $healthy) { Fail (Korean 'm43') 'Takeover health validation failed; rollback binaries retained. Inspect status before retrying.' }
    }
    if (-not $NoStart) {
        # Arm BEFORE the first stop: a multi-tray stop may fail part way through.
        $trayRepairRequired = $true
        foreach ($process in $oldTray) { StopOwned $process @($tray, ($tray + '.old-' + $stamp)) }
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $present = @($userPath -split ';' | Where-Object { [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\', '/') -ieq $InstallDir }).Count -gt 0
    if (-not $present) {
        $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { $userPath.TrimEnd(';') + ';' + $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Say (Korean 'm44') 'Added user PATH. Open a new terminal to pick it up.'
    }
    CompleteSetup
    $setupComplete = $true
} finally {
    if ($trayRepairRequired -and -not $NoStart) {
        try {
            $runningTray = RestoreInstallerTray $tray $fallbackTray
            $state = if ($setupComplete) { Message (Korean 'm45') 'Setup complete' } else { Message (Korean 'm46') 'Setup incomplete' }
            if ($runningTray) { Say "$state; $(Korean 'm47') $(Korean 'm4') $(Korean 'm48'): $runningTray" "$state; tray confirmed running: $runningTray" }
            else { Write-Warning (Message "$state; $(Korean 'm47') $(Korean 'm49') $(Korean 'm50') $(Korean 'm51'). $(Korean 'm52') $(Korean 'm53') $(Korean 'm54')." "$state; tray could not be confirmed running; start it manually.") -WarningAction Continue }
        } catch { Write-Warning (Message (Korean 'm58') 'Tray recovery failed; installation may be incomplete. Start the tray manually.') -WarningAction Continue }
    }
    try { if ($scratch -and [IO.Directory]::Exists($scratch)) { [IO.Directory]::Delete($scratch, $true) } }
    catch { Write-Warning (Message (Korean 'm55') 'Installer scratch cleanup skipped.') -WarningAction Continue }
    # Last even on failure: enumeration and each deletion are best effort.
    if ($retentionRequired) {
        try { RetainRollbackGenerations $InstallDir }
        catch { Write-Warning (Message (Korean 'm7') 'Rollback cleanup skipped; will retry next time.') -WarningAction Continue }
    }
}
