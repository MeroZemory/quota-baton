# Pester-free. No release binaries, network, registry writes or real profile writes.
[CmdletBinding()]
param(
    [string]$EvidenceDir = 'C:/_projects/SampleUser/quota-baton/.sandbox/reports/R17-evidence/install-smoke/v2/pwsh'
)
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
$installer = Join-Path $project 'install.ps1'
$uninstaller = Join-Path $project 'uninstall.ps1'
$EvidenceDir = [IO.Path]::GetFullPath($EvidenceDir)
[void][IO.Directory]::CreateDirectory($EvidenceDir)
$fixture = Join-Path $project ('.sandbox/install-smoke-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($fixture)
$saved = @{}
foreach ($name in @('USERPROFILE', 'LOCALAPPDATA', 'APPDATA', 'QB_HOME', 'PATH', 'TEMP', 'TMP')) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$smokeState = [pscustomobject]@{ Zip = $null; ApiCalls = @(); Downloads = @() }
$receipts = New-Object System.Collections.Generic.List[object]
$transcript = Join-Path $EvidenceDir 'transcript.txt'
Start-Transcript -LiteralPath $transcript -Force | Out-Null
function Assert($condition, [string]$message) { if (-not $condition) { throw $message } }
function Tree([string]$path) {
    @(Get-ChildItem -LiteralPath $path -File -Recurse | Sort-Object FullName | ForEach-Object { $_.FullName.Substring($path.Length) + ':' + (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }) -join "`n"
}
function Case([string]$name, [scriptblock]$body) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    try { & $body; $receipts.Add([pscustomobject]@{ name = $name; passed = $true; milliseconds = $timer.ElapsedMilliseconds }); Write-Host "PASS $name" }
    catch { $receipts.Add([pscustomobject]@{ name = $name; passed = $false; error = $_.Exception.Message }); throw }
}
# Scope-local tripwires are inherited by the invoked scripts. Merely detecting
# fake provider commands must never run them, even for --version.
function Get-CimInstance { throw 'Unexpected process inspection during dry mode' }
function Start-Process { throw 'Unexpected process start during dry mode' }
function Stop-Process { throw 'Unexpected process stop during dry mode' }
function Read-Host { throw 'Unexpected prompt during dry mode' }
function Invoke-RestMethod {
    param($Uri, $Headers)
    $smokeState.ApiCalls += $Uri
    return [pscustomobject]@{ assets = @(
        # Public v0.5.0 API metadata: quota-baton-0.5.0-windows-x64.zip
        [pscustomobject]@{ name = 'quota-baton-0.6.0-windows-x64.zip'; browser_download_url = 'https://fixture.invalid/release.zip' },
        [pscustomobject]@{ name = 'quota-baton-0.6.0-windows-x64.zip.sha256'; browser_download_url = 'https://fixture.invalid/release.zip.sha256' }
    ) }
}
function Invoke-WebRequest {
    param($Uri, $OutFile, [switch]$UseBasicParsing)
    $smokeState.Downloads += $Uri
    $source = if ($Uri.EndsWith('.sha256')) { $smokeState.Zip + '.sha256' } else { $smokeState.Zip }
    [IO.File]::Copy($source, $OutFile)
}
try {
    $env:USERPROFILE = Join-Path $fixture 'profile'
    $env:LOCALAPPDATA = Join-Path $env:USERPROFILE 'AppData/Local'
    $env:APPDATA = Join-Path $env:USERPROFILE 'AppData/Roaming'
    $env:QB_HOME = Join-Path $env:LOCALAPPDATA 'quota-baton'
    $env:TEMP = Join-Path $fixture 'scratch'
    $env:TMP = $env:TEMP
    $fake = Join-Path $fixture 'fake-bin'
    foreach ($path in @($env:QB_HOME, $env:APPDATA, $env:TEMP, $fake)) { [void][IO.Directory]::CreateDirectory($path) }
    $env:PATH = $fake
    foreach ($provider in @('codex', 'claude')) {
        [IO.File]::WriteAllText((Join-Path $fake ($provider + '.cmd')), "@echo off`r`necho EXECUTED> `"$fixture/provider-executed`"`r`nexit /b 97`r`n")
    }
    [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'config.toml'), "[ui]`nlanguage = 'en'`n")
    [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'keep.txt'), 'synthetic account data')
    $payload = Join-Path $fixture 'payload'
    [void][IO.Directory]::CreateDirectory($payload)
    foreach ($name in @('quota-baton.exe', 'quota-baton-tray.exe')) { [IO.File]::WriteAllText((Join-Path $payload $name), 'NONEXECUTABLE dry-mode sentinel') }
    # An archive script body must not be executed or extracted by a dry run.
    [IO.File]::WriteAllText((Join-Path $payload 'install.ps1'), "throw 'downloaded body executed'")
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $smokeState.Zip = Join-Path $fixture 'release.zip'
    [IO.Compression.ZipFile]::CreateFromDirectory($payload, $smokeState.Zip)
    $hash = (Get-FileHash -LiteralPath $smokeState.Zip -Algorithm SHA256).Hash
    [IO.File]::WriteAllText(($smokeState.Zip + '.sha256'), "$hash *release.zip`n")
    $install = Join-Path $fixture 'install'
    $userPathBefore = [Environment]::GetEnvironmentVariable('Path', 'User')
    $profileBefore = Tree $env:USERPROFILE
    Case 'parse-install-and-uninstall' {
        foreach ($file in @($installer, $uninstaller)) {
            $tokens = $null; $errors = $null
            $ast = [Management.Automation.Language.Parser]::ParseFile($file, [ref]$tokens, [ref]$errors)
            Assert ($errors.Count -eq 0) "Parser errors: $file"
            Assert ($null -ne $ast.ParamBlock) "Leading param block missing: $file"
        }
    }
    Case 'local-whatif-nostart-yes-login-detects-without-execution' {
        $output = (& $installer -ZipPath $smokeState.Zip -InstallDir $install -NoStart -Yes -Login -WhatIf 6>&1 | Out-String -Width 4096)
        Assert ($output -match 'SHA256 verified') 'Checksum not verified'
        Assert ($output -match 'codex detected:.*fake-bin' -and $output -match 'claude detected:.*fake-bin') "Fake commands not detected: $output"
        Assert ($output -match 'Dry run:') 'Config ui.language was not honored'
        Assert (-not (Test-Path -LiteralPath $install)) 'Dry run created install directory'
    }
    Case 'whatif-without-nostart-and-explicit-korean' {
        $output = (& $installer -LocalZipPath $smokeState.Zip -LocalChecksumPath ($smokeState.Zip + '.sha256') -InstallDir $install -Lang ko -WhatIf 6>&1 | Out-String -Width 4096)
        Assert ($output -match '미리보기:' -and $output -match 'codex 발견:') 'Lang override or aliases failed'
    }
    Case 'checksum-mismatch-refused-even-whatif' {
        $bad = Join-Path $fixture 'bad.sha256'
        [IO.File]::WriteAllText($bad, ('0' * 64))
        $refused = $false
        try { & $installer -ZipPath $smokeState.Zip -ChecksumPath $bad -InstallDir $install -NoStart -WhatIf }
        catch { $refused = $_.Exception.Message -match 'SHA256 checksum mismatch' }
        Assert $refused 'Checksum mismatch was not refused'
    }
    Case 'malformed-checksum-refused' {
        $bad = Join-Path $fixture 'bad.sha256'
        [IO.File]::WriteAllText($bad, "$hash`n$hash")
        $refused = $false
        try { & $installer -ZipPath $smokeState.Zip -ChecksumPath $bad -InstallDir $install -WhatIf }
        catch { $refused = $_.Exception.Message -match 'Invalid SHA256' }
        Assert $refused 'Multi-record sidecar was accepted'
    }
    Case 'missing-payload-refused-before-install' {
        $empty = Join-Path $fixture 'empty.zip'
        $only = Join-Path $fixture 'only'
        [void][IO.Directory]::CreateDirectory($only)
        [IO.File]::WriteAllText((Join-Path $only 'quota-baton.exe'), 'sentinel')
        [IO.Compression.ZipFile]::CreateFromDirectory($only, $empty)
        [IO.File]::WriteAllText(($empty + '.sha256'), (Get-FileHash -LiteralPath $empty).Hash)
        $refused = $false
        try { & $installer -ZipPath $empty -InstallDir $install -WhatIf }
        catch { $refused = $_.Exception.Message -match 'Missing or ambiguous payload' }
        Assert $refused 'Missing tray was accepted'
    }
    Case 'latest-and-version-download-resolution-mocked' {
        $smokeState.ApiCalls = @(); $smokeState.Downloads = @()
        & $installer -InstallDir $install -WhatIf
        & $installer -Version 0.6.0 -InstallDir $install -WhatIf
        Assert ($smokeState.ApiCalls.Count -eq 2 -and $smokeState.ApiCalls[0].EndsWith('/latest') -and $smokeState.ApiCalls[1].EndsWith('/tags/v0.6.0')) 'Release resolution incorrect'
        Assert ($smokeState.Downloads.Count -eq 4) 'Zip and sidecar were not fetched'
        Assert (@(Get-ChildItem -LiteralPath $env:TEMP).Count -eq 0) 'Download staging leaked'
    }
    Case 'native-claude-fallback-without-client-execution' {
        [IO.File]::Move((Join-Path $fake 'claude.cmd'), (Join-Path $fake 'claude.hidden'))
        $nativeDir = Join-Path $env:USERPROFILE '.local/bin'
        [void][IO.Directory]::CreateDirectory($nativeDir)
        [IO.File]::WriteAllText((Join-Path $nativeDir 'claude.exe'), 'NONEXECUTABLE native sentinel')
        function claude { throw 'Source-less command must never execute' }
        $output = (& $installer -ZipPath $smokeState.Zip -InstallDir $install -WhatIf 6>&1 | Out-String -Width 4096)
        Assert ($output -match 'claude detected:.*\.local') 'Native Claude fallback not detected'
        [IO.File]::Delete((Join-Path $nativeDir 'claude.exe'))
    }
    Case 'missing-clients-and-explicit-english' {
        [IO.File]::Move((Join-Path $fake 'codex.cmd'), (Join-Path $fake 'codex.hidden'))
        [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'config.toml'), "[ui]`nlanguage = 'ko'`n")
        $output = (& $installer -ZipPath $smokeState.Zip -InstallDir $install -Lang en -WhatIf 6>&1 | Out-String -Width 4096)
        Assert ($output -match 'codex not found' -and $output -match 'claude not found') 'Missing client detection or explicit English failed'
        [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'config.toml'), "[ui]`nlanguage = 'en'`n")
    }
    Case 'bootstrap-invoke-expression-whatif' {
        & {
            $WhatIfPreference = $true
            Invoke-Expression ([IO.File]::ReadAllText($installer))
        }
        Assert (-not (Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'Programs/quota-baton'))) 'IEX dry mode created install'
    }
    Case 'uninstall-whatif-keeps-install-and-data' {
        [void][IO.Directory]::CreateDirectory($install)
        [IO.File]::WriteAllText((Join-Path $install 'quota-baton.exe'), 'NONEXECUTABLE uninstall sentinel')
        $before = Tree $install
        & $uninstaller -InstallDir $install -WhatIf
        & $uninstaller -InstallDir $install -RemoveData -Lang en -WhatIf
        Assert ((Tree $install) -eq $before) 'Uninstall dry run changed installation'
    }
    Case 'snapshot-dto-shape-and-field-validation' {
        $tokens = $null; $errors = $null
        $ast = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
        $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'SnapshotHealth' }, $true)
        . ([scriptblock]::Create($definition.Extent.Text))
        $json = '{"schema_version":1,"health":{"ok":true,"version":"0.6.0","pid":710,"started_at":"2026-10-27T00:00:00Z"}}'
        $health = SnapshotHealth $json
        Assert ($health.ok -and $health.pid -eq 710 -and $null -ne $health.started_at) 'Actual SnapshotBundleV1.health shape not decoded'
        foreach ($bad in @($json.Replace('"pid":710', '"pid":"710"'), $json.Replace('"ok":true', '"ok":"true"'), $json.Replace('"pid":710', '"pid":0'), $json.Replace('2026-10-27T00:00:00Z', 'invalid-date'), $json.Replace('"schema_version":1', '"schema_version":2'))) {
            Assert ($null -eq (SnapshotHealth $bad)) 'Invalid health DTO was accepted'
        }
    }
    Case 'setup-workflow-order-nostart-no-clients-and-accounts-mocked' {
        # Load the actual workflow functions, leaving all IO at mocked boundaries.
        # This exercises real setup control flow without PATH/registry/install IO.
        $tokens = $null; $errors = $null
        $ast = [Management.Automation.Language.Parser]::ParseFile($installer, [ref]$tokens, [ref]$errors)
        foreach ($name in @('Message', 'Fail', 'Say', 'WaitCoreReady', 'EnsureCoreReady', 'CompleteSetup')) {
            $definition = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name }, $true)
            . ([scriptblock]::Create($definition.Extent.Text))
        }
        $setupState = [pscustomobject]@{ Events = (New-Object 'System.Collections.Generic.List[string]'); Running = $false; Healthy = $true; Accounts = @() }
        $exe = Join-Path $install 'quota-baton.exe'; $tray = Join-Path $install 'quota-baton-tray.exe'
        $Lang = 'en'; $Yes = $true; $NoStart = $false; $Login = $false
        $detected = @{ codex = $true; claude = $true }
        function OwnedProcesses([string]$path) { if ($path -eq $exe -and $setupState.Running) { return [pscustomobject]@{ ProcessId = 710 } } }
        function ReadHealth { $setupState.Events.Add('health'); return [pscustomobject]@{ ok = $setupState.Healthy; pid = 710; started_at = '2026-10-27T00:00:00Z' } }
        function Start-Process {
            param($FilePath, $ArgumentList, [switch]$PassThru, $WindowStyle)
            Assert ($WindowStyle -eq 'Hidden') 'Start must be hidden'
            if ($FilePath -eq $exe) { $setupState.Events.Add('serve'); $setupState.Running = $true; return [pscustomobject]@{ Id = 710; HasExited = (-not $setupState.Healthy) } }
            $setupState.Events.Add('tray')
        }
        function RunCli([string[]]$Arguments) {
            if ($Arguments[0] -eq 'wire') {
                if ($Arguments -contains '--apply') { Assert ($setupState.Running -and $setupState.Healthy) 'Wire apply before healthy core'; $setupState.Events.Add('apply') }
                else { $setupState.Events.Add('preview') }
            } elseif ($Arguments[0] -eq 'accounts' -and $Arguments[1] -eq 'list') { $setupState.Events.Add('accounts'); return $setupState.Accounts }
            elseif ($Arguments[0] -eq 'accounts') { $setupState.Events.Add('login:' + $Arguments[2]) }
            else { $setupState.Events.Add('status') }
        }
        CompleteSetup
        Assert (($setupState.Events -join ',') -eq 'preview,serve,health,apply,tray,accounts,status') 'Fresh setup order incorrect'
        $setupState.Events.Clear(); $setupState.Running = $true
        CompleteSetup
        Assert (($setupState.Events -join ',') -eq 'preview,health,apply,tray,accounts,status') 'Existing core was restarted or not validated'
        $setupState.Events.Clear(); $setupState.Running = $false; $NoStart = $true; $Login = $true
        $output = (CompleteSetup 6>&1 | Out-String -Width 4096)
        Assert (($setupState.Events -join ',') -eq 'preview,accounts,status') 'NoStart caused apply, login or start'
        Assert ($output -match 'skipping wire apply and login') 'NoStart skipped effects silently'
        $setupState.Events.Clear(); $NoStart = $false; $Login = $false; $detected = @{ codex = $false; claude = $false }
        $output = (CompleteSetup 6>&1 | Out-String -Width 4096)
        Assert (($setupState.Events -join ',') -eq 'tray,accounts,status' -and $output -match 'No clients detected; skipping wire') 'No clients caused refused wire none/none'
        $setupState.Events.Clear(); $detected = @{ codex = $true; claude = $true }; $setupState.Healthy = $false
        $refused = $false
        try { CompleteSetup } catch { $refused = $_.Exception.Message -match 'Fresh core readiness failed' }
        Assert ($refused -and -not $setupState.Events.Contains('apply')) 'Unhealthy startup still applied wire'
        $setupState.Events.Clear(); $setupState.Healthy = $true; $setupState.Running = $true; $Login = $true
        $setupState.Accounts = @("id`tprovider`temail", "cx-1`tcodex`tsynthetic@example.invalid", 'claude summary (not a TSV account)')
        CompleteSetup
        Assert (-not $setupState.Events.Contains('login:codex') -and $setupState.Events.Contains('login:claude')) 'Provider was not read from second TSV column'
    }
    Case 'no-profile-path-process-or-provider-effects' {
        Assert ((Tree $env:USERPROFILE) -eq $profileBefore) 'Profile contents changed'
        Assert ([Environment]::GetEnvironmentVariable('Path', 'User') -ceq $userPathBefore) 'User PATH changed'
        Assert (-not (Test-Path -LiteralPath (Join-Path $fixture 'provider-executed'))) 'Provider command ran'
        Assert (@(Get-ChildItem -LiteralPath $env:TEMP).Count -eq 0) 'Scratch leaked'
    }
} finally {
    $result = [ordered]@{
        command = 'pwsh -NoProfile -File tests/install-smoke.ps1'
        runtime = $PSVersionTable.PSVersion.ToString()
        installer_sha256 = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash
        uninstaller_sha256 = (Get-FileHash -LiteralPath $uninstaller -Algorithm SHA256).Hash
        harness_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash
        cases = @($receipts.ToArray())
        limitations = 'Dry script modes plus actual setup functions with mocked IO. Release metadata/downloads mocked in harness; no real install, registry, provider login or takeover. Runtime is recorded separately.'
    }
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $EvidenceDir 'result.json') -Encoding UTF8
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    Stop-Transcript | Out-Null
    # Only this run's resolved, unique sandbox path is disposable.
    $sandbox = [IO.Path]::GetFullPath((Join-Path $project '.sandbox')) + [IO.Path]::DirectorySeparatorChar
    if (-not ([IO.Path]::GetFullPath($fixture)).StartsWith($sandbox, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe fixture cleanup path' }
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
