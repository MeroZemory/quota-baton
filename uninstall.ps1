[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\quota-baton'),
    [switch]$RemoveData,
    [ValidateSet('auto', 'ko', 'en')][string]$Lang = 'auto'
)
$ErrorActionPreference = 'Stop'
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
    if ($path -eq [IO.Path]::GetPathRoot($path).TrimEnd('\')) { Fail '드라이브 루트는 제거할 수 없습니다.' 'Refusing a drive root.' }
    if (Test-Path -LiteralPath $path) {
        $item = Get-Item -LiteralPath $path
        while ($item) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Fail '재분석 경로는 제거할 수 없습니다.' 'Refusing a reparse path.' }
            $item = $item.Parent
        }
    }
}
if (-not $RemoveData -and ($dataDir -eq $InstallDir -or $dataDir.StartsWith($InstallDir + '\', [StringComparison]::OrdinalIgnoreCase))) { Fail '설치 경로 안에 보존할 계정 데이터가 있어 제거하지 않았습니다.' 'Data is inside InstallDir; refusing to remove kept data.' }
if (-not $PSCmdlet.ShouldProcess($InstallDir, (Message 'unwire, 설치된 PID 종료, 설치 폴더 및 사용자 PATH 항목 제거' 'Unwire, stop exact installed PIDs, remove installation and its user PATH entry'))) { return }
$exe = Join-Path $InstallDir 'quota-baton.exe'
if (Test-Path -LiteralPath $InstallDir) {
    if (-not (Test-Path -LiteralPath $exe)) { Fail '설치된 quota-baton.exe가 없어 unwire를 실행하지 못했습니다. 설치 폴더를 보존했습니다.' 'Cannot unwire: installed quota-baton.exe is missing; installation retained.' }
    & $exe unwire --apply
    if ($LASTEXITCODE -ne 0) { Fail 'unwire 실패. 설치 폴더를 보존했습니다.' 'Unwire failed; installation retained.' }
    # Tray first, including our renamed rollback images. Never kill by image name.
    foreach ($name in @('quota-baton-tray.exe', 'quota-baton.exe')) {
        $paths = @((Join-Path $InstallDir $name)) + @(Get-ChildItem -LiteralPath $InstallDir -File | Where-Object { $_.Name -match ('^' + [regex]::Escape($name) + '\.old-\d{21}$') } | ForEach-Object { $_.FullName })
        $processes = @(Get-CimInstance Win32_Process -Filter "Name LIKE 'quota-baton%'" | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -in $paths })
        foreach ($process in $processes) {
            $current = Get-CimInstance Win32_Process -Filter "ProcessId = $($process.ProcessId)"
            if ($current -and $current.CreationDate -eq $process.CreationDate -and $current.ExecutablePath -in $paths) {
                Stop-Process -Id $current.ProcessId -Force
                Get-Process -Id $current.ProcessId -ErrorAction SilentlyContinue | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
                if (Get-CimInstance Win32_Process -Filter "ProcessId = $($current.ProcessId)" | Where-Object { $_.CreationDate -eq $current.CreationDate }) { Fail '설치된 프로세스가 종료되지 않아 설치 폴더를 보존했습니다.' 'Installed process did not stop; installation retained.' }
            }
        }
    }
    Remove-Item -LiteralPath $InstallDir -Recurse -Force
}
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';')
$kept = @($entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_.Trim().Trim('"')).TrimEnd('\', '/') -ine $InstallDir })
if ($kept.Count -ne $entries.Count) { [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User') }
if ($RemoveData) {
    if ($PSCmdlet.ShouldProcess($dataDir, (Message 'quota-baton 계정 데이터 제거' 'Remove quota-baton account data')) -and (Test-Path -LiteralPath $dataDir)) { Remove-Item -LiteralPath $dataDir -Recurse -Force }
} else { Say "계정 데이터 유지: $dataDir" "Account data kept: $dataDir" }
Say '제거 완료.' 'Uninstall complete.'
