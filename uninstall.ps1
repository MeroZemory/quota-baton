[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\quota-baton'),
    [switch]$RemoveData,
    [ValidateSet('auto', 'ko', 'en')][string]$Lang = 'auto'
)
$ErrorActionPreference = 'Stop'
function Korean([string]$key) {
    $text = @{
        m1 = '65Oc65287J2067iMIOujqO2KuOuKlCDsoJzqsbDtlaAg7IiYIOyXhuyKteuLiOuLpC4='
        m2 = '7J6s67aE7ISdIOqyveuhnOuKlCDsoJzqsbDtlaAg7IiYIOyXhuyKteuLiOuLpC4='
        m3 = '7ISk7LmYIOqyveuhnCDslYjsl5Ag67O07KG07ZWgIOqzhOyglSDrjbDsnbTthLDqsIAg7J6I7Ja0IOygnOqxsO2VmOyngCDslYrslZjsirXri4jri6Qu'
        m4 = 'dW53aXJlLCDshKTsuZjrkJwgUElEIOyiheujjCwg7ISk7LmYIO2PtOuNlCDrsI8g7IKs7Jqp7J6QIFBBVEgg7ZWt66qpIOygnOqxsA=='
        m5 = '7ISk7LmY65CcIHF1b3RhLWJhdG9uLmV4ZeqwgCDsl4bslrQgdW53aXJl66W8IOyLpO2Wie2VmOyngCDrqrvtlojsirXri4jri6QuIOyEpOy5mCDtj7TrjZTrpbwg67O07KG07ZaI7Iq164uI64ukLg=='
        m6 = 'dW53aXJlIOyLpO2MqC4g7ISk7LmYIO2PtOuNlOulvCDrs7TsobTtlojsirXri4jri6Qu'
        m7 = '7ISk7LmY65CcIO2UhOuhnOyEuOyKpOqwgCDsooXro4zrkJjsp4Ag7JWK7JWEIOyEpOy5mCDtj7TrjZTrpbwg67O07KG07ZaI7Iq164uI64ukLg=='
        m8 = '7KCc6rGw'
        m9 = '66+47JmE66OM'
        m10 = '64Ko7J2A'
        m11 = '7ISk7LmY'
        m12 = '7YyM7J28'
        m13 = 'cXVvdGEtYmF0b24g6rOE7KCVIOuNsOydtO2EsCDsoJzqsbA='
        m14 = '6rOE7KCV'
        m15 = '642w7J207YSw'
        m16 = '7Jyg7KeA'
        m17 = '7KCc6rGwIOyZhOujjC4='
    }
    return [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($text[$key]))
}
function Message([string]$ko, [string]$en) { if ($Lang -eq 'en') { return $en } else { return $ko } }
function Fail([string]$ko, [string]$en) { throw (Message $ko $en) }
function Say([string]$ko, [string]$en) { if ($Lang -eq 'en') { Write-Host $en } else { Write-Host $ko } }
$dataDir = if ($env:QB_HOME) { $env:QB_HOME } else { Join-Path $env:LOCALAPPDATA 'quota-baton' }
if ($Lang -eq 'auto') {
    $Lang = 'ko'
    $config = Join-Path $dataDir 'config.toml'
    if (Test-Path -LiteralPath $config) {
        $section = ''
        foreach ($line in [IO.File]::ReadAllLines($config)) {
            if ($line -match '^\s*\[([^]]+)\]') { $section = $Matches[1] }
            if ($section -eq 'ui' -and $line -match '^\s*language\s*=\s*["''](en|ko)["'']') { $Lang = $Matches[1]; break }
        }
    }
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\', '/')
$dataDir = [IO.Path]::GetFullPath($dataDir).TrimEnd('\', '/')
foreach ($path in @($InstallDir, $dataDir)) {
    if ($path -eq [IO.Path]::GetPathRoot($path).TrimEnd('\')) { Fail (Korean 'm1') 'Refusing a drive root.' }
    if (Test-Path -LiteralPath $path) {
        $item = Get-Item -LiteralPath $path
        while ($item) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail (Korean 'm2') 'Refusing a reparse path.' }
            $item = $item.Parent
        }
    }
}
if (-not $RemoveData -and ($dataDir -eq $InstallDir -or $dataDir.StartsWith($InstallDir + '\', [StringComparison]::OrdinalIgnoreCase))) { Fail (Korean 'm3') 'Data is inside InstallDir; refusing to remove kept data.' }
if (-not $PSCmdlet.ShouldProcess($InstallDir, (Message (Korean 'm4') 'Unwire, stop exact installed PIDs, remove installation and its user PATH entry'))) { return }
$exe = Join-Path $InstallDir 'quota-baton.exe'
if (Test-Path -LiteralPath $InstallDir) {
    if (-not (Test-Path -LiteralPath $exe)) { Fail (Korean 'm5') 'Cannot unwire: installed quota-baton.exe is missing; installation retained.' }
    & $exe unwire --apply
    if ($LASTEXITCODE -ne 0) { Fail (Korean 'm6') 'Unwire failed; installation retained.' }
    # Tray first, including our renamed rollback images. Never kill by image name.
    foreach ($name in @('quota-baton-tray.exe', 'quota-baton.exe')) {
        $paths = @((Join-Path $InstallDir $name)) + @(Get-ChildItem -LiteralPath $InstallDir -File | Where-Object { $_.Name -match ('^' + [regex]::Escape($name) + '\.old-\d{21}$') } | ForEach-Object { $_.FullName })
        $processes = @(Get-CimInstance Win32_Process -Filter "Name LIKE 'quota-baton%'" | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -in $paths })
        foreach ($process in $processes) {
            $current = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)"
            if ($current -and $current.CreationDate -eq $process.CreationDate -and $current.ExecutablePath -in $paths) {
                Stop-Process -Id $current.ProcessId -Force
                Get-Process -Id $current.ProcessId -ErrorAction SilentlyContinue | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
                if (Get-CimInstance Win32_Process -Filter "ProcessId = $($current.ProcessId)" | Where-Object { $_.CreationDate -eq $current.CreationDate }) { Fail (Korean 'm7') 'Installed process did not stop; installation retained.' }
            }
        }
    }
    try { Remove-Item -LiteralPath $InstallDir -Recurse -Force }
    catch {
        Write-Warning (Message "$(Korean 'm8') $(Korean 'm9'). $(Korean 'm10') $(Korean 'm11') $(Korean 'm12'): $InstallDir" "Uninstall incomplete; residual installation: $InstallDir") -WarningAction Continue
        throw
    }
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';')
$kept = @($entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\', '/') -ine $InstallDir })
if ($kept.Count -ne $entries.Count) { [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User') }
if ($RemoveData) {
    if ($PSCmdlet.ShouldProcess($dataDir, (Message (Korean 'm13') 'Remove quota-baton account data')) -and (Test-Path -LiteralPath $dataDir)) { Remove-Item -LiteralPath $dataDir -Recurse -Force }
} else { Say "$(Korean 'm14') $(Korean 'm15') $(Korean 'm16'): $dataDir" "Account data kept: $dataDir" }
Say (Korean 'm17') 'Uninstall complete.'
