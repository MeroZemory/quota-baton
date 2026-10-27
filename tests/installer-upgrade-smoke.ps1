# Synthetic full-workflow smoke. No real registry, user PATH, or process mutation.
[CmdletBinding()]
param([string]$EvidenceDir = 'C:/_projects/SampleUser/quota-baton/.sandbox/reports/R17.1-evidence/upgrade/pwsh')
$ErrorActionPreference = 'Stop'
$project = Split-Path $PSScriptRoot -Parent
$installer = Join-Path $project 'install.ps1'
$uninstaller = Join-Path $project 'uninstall.ps1'
$EvidenceDir = [IO.Path]::GetFullPath($EvidenceDir)
[void][IO.Directory]::CreateDirectory($EvidenceDir)
$fixture = Join-Path $project ('.sandbox/installer-upgrade-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($fixture)
$saved = @{}
foreach ($name in @('USERPROFILE','LOCALAPPDATA','APPDATA','QB_HOME','PATH','PATHEXT','TEMP','TMP')) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$receipts = New-Object 'System.Collections.Generic.List[object]'
$locked = $null
$junctions = New-Object 'System.Collections.Generic.List[string]'
Start-Transcript -LiteralPath (Join-Path $EvidenceDir 'transcript.txt') -Force | Out-Null
function Assert($condition, [string]$message) { if (-not $condition) { throw $message } }
function Contained([string]$path) {
    $absolute = [IO.Path]::GetFullPath($path)
    $prefix = [IO.Path]::GetFullPath($fixture).TrimEnd('\') + '\'
    Assert ($absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) "Outside fixture: $path"
    return $absolute
}
function Case([string]$name, [scriptblock]$body) {
    try { & $body; $receipts.Add([pscustomobject]@{ name=$name; passed=$true }); Write-Host "PASS $name" }
    catch { $receipts.Add([pscustomobject]@{ name=$name; passed=$false; error=$_.Exception.Message }); throw }
}
function MockUserPath { return $scenario.UserPath }
function SetMockUserPath([string]$value) { $scenario.UserPath=$value; $scenario.Events.Add('user-path') }
function FindWireClaude {
    $extensions=if ($null -eq $env:PATHEXT) { '.COM;.EXE;.BAT;.CMD' } else { $env:PATHEXT }
    foreach ($directory in ($env:PATH -split ';')) {
        if ([string]::IsNullOrWhiteSpace($directory)) { continue }
        $directory=$directory.Trim('"')
        if (-not [IO.Path]::IsPathRooted($directory)) { continue }
        foreach ($extension in ($extensions -split ';')) {
            if ($extension -notmatch '^\.(COM|EXE|BAT|CMD)$') { continue }
            $candidate=Join-Path $directory ('claude'+$extension)
            if ([IO.File]::Exists($candidate)) { return [IO.Path]::GetFullPath($candidate) }
        }
    }
    return $null
}
function MockHealth {
    $core = @($scenario.Processes | Where-Object { $_.ExecutablePath -eq $scenario.Exe } | Select-Object -Last 1)
    if ($core.Count) { return [pscustomobject]@{ ok=$true; pid=$core[0].ProcessId; started_at=$core[0].CreationDate } }
}
function MockCli([string[]]$Arguments) {
    $global:LASTEXITCODE = 0
    $scenario.Events.Add('cli:' + ($Arguments -join ' '))
    if ($Arguments[0] -eq 'wire') {
        Assert ([IO.File]::Exists((Join-Path $env:APPDATA 'npm/codex.cmd'))) 'Fixed Codex wire target is missing'
        $claude=FindWireClaude
        Assert ([string]::Equals($claude,$scenario.Targets.claude,[StringComparison]::OrdinalIgnoreCase)) "Wire environment resolves wrong Claude target: $claude"
        if ($scenario.Failure -eq 'wire' -and $Arguments -contains '--apply') { throw 'synthetic wire apply failure' }
    }
    if ($scenario.Failure -eq 'late' -and $Arguments[0] -eq 'accounts') { throw 'synthetic late setup failure' }
    if ($Arguments[0] -eq 'accounts') { return @('id' + "`tprovider`temail", 'cx-1' + "`tcodex`tfixture@example.test", 'cl-1' + "`tclaude`tfixture@example.test") }
}
function CopyFixturePayload([string]$source, [string]$destination) {
    [void](Contained $source); [void](Contained $destination)
    if ($scenario.Failure -eq 'swap' -and $destination.EndsWith('quota-baton-tray.exe')) { throw 'synthetic tray copy failure' }
    [IO.File]::Copy($source, $destination)
}
function Get-CimInstance {
    param($ClassName, $Filter)
    if ($Filter -match '^ProcessId = (\d+)$') { return @($scenario.Processes | Where-Object { $_.ProcessId -eq [int]$Matches[1] }) }
    Assert ($ClassName -eq 'Win32_Process') 'Unexpected CIM class'
    return @($scenario.Processes.ToArray())
}
function Start-Process {
    param($FilePath, $ArgumentList, [switch]$PassThru, $WindowStyle)
    [void](Contained $FilePath)
    Assert ($WindowStyle -eq 'Hidden') 'Process start was not hidden'
    Assert ((Split-Path $FilePath -Leaf) -match '^quota-baton(?:-tray)?\.exe(?:\.old-\d{21})?$') 'Unexpected synthetic process executable'
    $scenario.NextPid++
    $id = $scenario.NextPid
    $record = [pscustomobject]@{ ProcessId=$id; ExecutablePath=$FilePath; CreationDate=('2030-01-01T00:00:' + $id); Id=$id; HasExited=$false }
    $record | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value { param($milliseconds) return $false }
    $scenario.Processes.Add($record)
    $scenario.Events.Add('start:' + (Split-Path $FilePath -Leaf))
    return $record
}
function Stop-Process {
    param($Id, [switch]$Force)
    $found = @($scenario.Processes | Where-Object { $_.ProcessId -eq $Id })
    Assert ($found.Count -eq 1 -and $Id -ne 999) 'Unexpected process stop'
    [void](Contained $found[0].ExecutablePath)
    $scenario.Events.Add('stop:' + (Split-Path $found[0].ExecutablePath -Leaf))
    [void]$scenario.Processes.Remove($found[0])
}
function Get-Process { param($Id, $ErrorAction) return $null }
function Wait-Process { param([Parameter(ValueFromPipeline=$true)]$InputObject, $Timeout) process {} }
function Start-Sleep { throw 'Unexpected readiness wait in synthetic workflow' }
function Read-Host { throw 'Unexpected prompt' }
function Invoke-RestMethod { throw 'Unexpected network request' }
function Invoke-WebRequest { throw 'Unexpected download' }

function Invoke-FixtureScript([string]$path, [hashtable]$parameters, [switch]$PlainIex, [bool]$IexWhatIf=$false) {
    $text = [IO.File]::ReadAllText($path)
    $tokens=$null; $errors=$null
    $ast = [Management.Automation.Language.Parser]::ParseInput($text, [ref]$tokens, [ref]$errors)
    Assert ($errors.Count -eq 0) 'Script parse failed before AST substitution'
    $edits = New-Object 'System.Collections.Generic.List[object]'
    if ($PlainIex) {
        # IEX has no argument binding: substitute fixture parameter defaults only.
        # Keep the complete CmdletBinding/param source and the real workflow.
        foreach ($parameter in $ast.ParamBlock.Parameters) {
            $name=$parameter.Name.VariablePath.UserPath
            if (-not $parameters.ContainsKey($name)) { continue }
            $value='$__iexFixtureParameters[''' + $name + ''']'
            if ($parameter.DefaultValue) {
                $edits.Add([pscustomobject]@{Start=$parameter.DefaultValue.Extent.StartOffset;End=$parameter.DefaultValue.Extent.EndOffset;Text=$value})
            } else {
                $edits.Add([pscustomobject]@{Start=$parameter.Extent.StartOffset;End=$parameter.Extent.EndOffset;Text=($parameter.Extent.Text+' = '+$value)})
            }
        }
        $assertNull="`nif (`$null -ne `$PSCmdlet) { throw 'IEX unexpectedly inherited PSCmdlet' }; `$scenario.Events.Add('plain-iex-null')`n"
        $edits.Add([pscustomobject]@{Start=$ast.ParamBlock.Extent.EndOffset;End=$ast.ParamBlock.Extent.EndOffset;Text=$assertNull})
    }
    foreach ($definition in $ast.FindAll({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'ReadHealth' }, $true)) {
        $replacement = 'function ReadHealth { MockHealth }'
        $edits.Add([pscustomobject]@{ Start=$definition.Extent.StartOffset; End=$definition.Extent.EndOffset; Text=$replacement })
    }
    foreach ($node in $ast.FindAll({ param($n) $n -is [Management.Automation.Language.InvokeMemberExpressionAst] -or $n -is [Management.Automation.Language.CommandAst] }, $true)) {
        if (@($edits | Where-Object { $node.Extent.StartOffset -ge $_.Start -and $node.Extent.EndOffset -le $_.End }).Count) { continue }
        $replacement=$null
        if ($node -is [Management.Automation.Language.InvokeMemberExpressionAst]) {
            $expression=$node.Expression.Extent.Text
            $member=$node.Member.Extent.Text
            if ($expression -eq '[Environment]' -and $node.Extent.Text -match '[''"]User[''"]') {
                if ($member -eq 'GetEnvironmentVariable') { $replacement='(MockUserPath)' }
                elseif ($member -eq 'SetEnvironmentVariable') { $replacement='(SetMockUserPath ' + $node.Arguments[1].Extent.Text + ')' }
            }
            if ($expression -eq '[IO.File]' -and $member -eq 'Copy' -and $node.Arguments[0].Extent.Text -eq '$staged') { $replacement='(CopyFixturePayload $staged $destination)' }
        } elseif ($node.InvocationOperator -eq 'Ampersand' -and $node.CommandElements[0].Extent.Text -eq '$exe') {
            if ($node.Extent.Text -match 'unwire --apply') { $replacement="MockCli @('unwire','--apply')" }
            elseif ($node.Extent.Text -match '@Arguments') { $replacement='MockCli $Arguments' }
            else { throw ('Unexpected direct native invocation: ' + $node.Extent.Text) }
        }
        if ($null -ne $replacement) { $edits.Add([pscustomobject]@{ Start=$node.Extent.StartOffset; End=$node.Extent.EndOffset; Text=$replacement }) }
    }
    foreach ($edit in @($edits | Sort-Object Start -Descending)) { $text=$text.Remove($edit.Start, $edit.End-$edit.Start).Insert($edit.Start, $edit.Text) }
    Assert ($text -notmatch '\[Environment\]::(?:Get|Set)EnvironmentVariable\([^\r\n]*[''\"]User[''\"]') 'Unmocked user PATH access'
    $scenario.Events.Add('invoke:' + (Split-Path $path -Leaf))
    if ($PlainIex) {
        $__iexFixtureParameters=$parameters
        $PSCmdlet=$null
        $WhatIfPreference=$IexWhatIf
        Invoke-Expression $text
    } else {
        & ([scriptblock]::Create($text)) @parameters
    }
}

function New-Scenario([string]$name, [string]$failure='') {
    $directory=Join-Path $fixture $name
    [void][IO.Directory]::CreateDirectory($directory)
    foreach ($name in @('quota-baton.exe','quota-baton-tray.exe')) { [IO.File]::WriteAllText((Join-Path $directory $name), 'old:' + $name) }
    $state=[pscustomobject]@{ Install=$directory; Exe=(Join-Path $directory 'quota-baton.exe'); Tray=(Join-Path $directory 'quota-baton-tray.exe'); Failure=$failure; UserPath='C:\synthetic-unrelated'; Targets=$realTargets; InputPath=$env:PATH; InputPathExt=$env:PATHEXT; InputPathExtPresent=(Test-Path Env:PATHEXT); LockedPath=$null; LockHandle=$null; LockReleased=$false; NextPid=100; Events=(New-Object 'System.Collections.Generic.List[string]'); Processes=(New-Object 'System.Collections.Generic.List[object]') }
    $state.Processes.Add([pscustomobject]@{ ProcessId=10; ExecutablePath=$state.Exe; CreationDate='old-core' })
    $state.Processes.Add([pscustomobject]@{ ProcessId=11; ExecutablePath=$state.Tray; CreationDate='old-tray' })
    $state.Processes.Add([pscustomobject]@{ ProcessId=999; ExecutablePath='C:\unrelated\quota-baton.exe'; CreationDate='unrelated' })
    return $state
}
try {
    $env:USERPROFILE=Join-Path $fixture 'profile'
    $env:LOCALAPPDATA=Join-Path $env:USERPROFILE 'AppData/Local'
    $env:APPDATA=Join-Path $env:USERPROFILE 'AppData/Roaming'
    $env:QB_HOME=Join-Path $env:LOCALAPPDATA 'quota-baton'
    $env:TEMP=Join-Path $fixture 'scratch'; $env:TMP=$env:TEMP
    foreach ($directory in @($env:QB_HOME,$env:APPDATA,$env:TEMP)) { [void][IO.Directory]::CreateDirectory($directory) }
    [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'keep.txt'),'synthetic-account-data')
    $herdrBin=Join-Path $env:APPDATA 'herdr/bin'
    $npmBin=Join-Path $env:APPDATA 'npm'
    $nativeBin=Join-Path $env:USERPROFILE '.local/bin'
    foreach ($directory in @($herdrBin,$npmBin,$nativeBin)) { [void][IO.Directory]::CreateDirectory($directory) }
    $realTargets=@{codex=(Join-Path $npmBin 'codex.cmd'); claude=(Join-Path $nativeBin 'claude.exe')}
    [IO.File]::WriteAllText($realTargets.codex,'@echo off' + "`r`nexit /b 97`r`n")
    [IO.File]::WriteAllText($realTargets.claude,'NONEXECUTABLE native client sentinel')
    [IO.File]::WriteAllText((Join-Path $nativeBin 'claude.com'),'NONEXECUTABLE wrong extension sentinel')
    foreach ($provider in @('codex','claude')) {
        [IO.File]::WriteAllText((Join-Path $herdrBin ($provider+'.ps1')), 'throw "Herdr wrapper must never execute"')
        [IO.File]::WriteAllText((Join-Path $herdrBin ($provider+'.cmd')), '@echo off' + "`r`nexit /b 97`r`n")
    }
    $env:PATH=$herdrBin+';'+$npmBin
    $payload=Join-Path $fixture 'payload'; [void][IO.Directory]::CreateDirectory($payload)
    foreach ($name in @('quota-baton.exe','quota-baton-tray.exe')) { [IO.File]::WriteAllText((Join-Path $payload $name),'new:'+$name) }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip=Join-Path $fixture 'release.zip'; [IO.Compression.ZipFile]::CreateFromDirectory($payload,$zip)
    [IO.File]::WriteAllText(($zip+'.sha256'),(Get-FileHash -LiteralPath $zip).Hash)
    Case 'safe-client-refuses-ancestor-junction-to-unsafe-canonical-directory' {
        $tokens=$null; $errors=$null
        $ast=[Management.Automation.Language.Parser]::ParseFile($installer,[ref]$tokens,[ref]$errors)
        $definition=$ast.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'SafeClientFile'},$true)
        . ([scriptblock]::Create($definition.Extent.Text))
        $target=Join-Path $fixture 'unsafe&canonical'
        $alias=Join-Path $fixture 'safe-alias'
        [void](Contained $target); [void](Contained $alias)
        [void][IO.Directory]::CreateDirectory($target)
        [IO.File]::WriteAllText((Join-Path $target 'claude.exe'),'NONEXECUTABLE junction sentinel')
        New-Item -ItemType Junction -Path $alias -Target $target | Out-Null
        $junctions.Add($alias)
        Assert ($null -eq (SafeClientFile (Join-Path $alias 'claude.exe'))) 'Safe alias to reparse ancestor was accepted'
        [IO.Directory]::Delete($alias)
        [void]$junctions.Remove($alias)
        Assert (Test-Path -LiteralPath (Join-Path $target 'claude.exe')) 'Removing test junction traversed its target'
        $semicolon=Join-Path $fixture 'unsafe;path'
        [void][IO.Directory]::CreateDirectory($semicolon)
        [IO.File]::WriteAllText((Join-Path $semicolon 'claude.exe'),'NONEXECUTABLE semicolon sentinel')
        Assert ($null -eq (SafeClientFile (Join-Path $semicolon 'claude.exe'))) 'Semicolon client directory accepted for scoped PATH'
    }
    Case 'quoted-semicolon-path-never-fabricates-client-directory' {
        $tokens=$null; $errors=$null
        $ast=[Management.Automation.Language.Parser]::ParseFile($installer,[ref]$tokens,[ref]$errors)
        foreach ($name in @('SafeClientFile','ClientPathDirectories','ResolveClient')) {
            $definition=$ast.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name},$true)
            . ([scriptblock]::Create($definition.Extent.Text))
        }
        $savedFixturePath=$env:PATH
        $savedFixtureExt=$env:PATHEXT
        $nativeHidden=$realTargets.claude+'.hidden'
        $fragment=Join-Path $fixture 'Tools'
        $official=Join-Path $fixture 'Official'
        foreach ($directory in @($fragment,$official)) { [void][IO.Directory]::CreateDirectory($directory); [IO.File]::WriteAllText((Join-Path $directory 'claude.exe'),'NONEXECUTABLE PATH parser sentinel') }
        try {
            [IO.File]::Move($realTargets.claude,$nativeHidden)
            $env:PATH='"'+$fragment+';Legacy";'+$official
            $env:PATHEXT='.EXE;.CMD'
            $resolved=ResolveClient 'claude'
            Assert ($resolved.Path -eq (Join-Path $official 'claude.exe')) 'Quoted semicolon PATH fabricated a directory fragment instead of selecting official fallback'
        } finally {
            if ([IO.File]::Exists($nativeHidden)) { [IO.File]::Move($nativeHidden,$realTargets.claude) }
            $env:PATH=$savedFixturePath; $env:PATHEXT=$savedFixtureExt
        }
    }
    Case 'codex-explicit-launcher-source-policy-unchanged' {
        $source=[IO.File]::ReadAllText((Join-Path $project 'crates/quota-baton/src/launcher_script.rs'))
        Assert ($source.Contains('launch-client --client codex --command') -and $source.Contains('%APPDATA%\\npm\\codex.cmd')) 'Codex explicit npm target source policy changed'
        Write-Host 'Source-policy check only: launcher generation and Rust wire were not executed.'
    }
    Case 'upgrade-locked-retention-keeps-newest-pair-and-cleans-other-generations' {
        $scenario=New-Scenario 'successful'
        $oldest='200001010000000000000'; $middle='200101010000000000000'
        foreach ($generation in @($oldest,$middle)) { foreach ($name in @('quota-baton.exe','quota-baton-tray.exe')) { [IO.File]::WriteAllText((Join-Path $scenario.Install ($name+'.old-'+$generation)),'old rollback') } }
        $lockedPath=Join-Path $scenario.Install ('quota-baton.exe.old-'+$oldest)
        $script:locked=[IO.File]::Open($lockedPath,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None)
        $deleteRefused=$false
        try { [IO.File]::Delete($lockedPath) } catch { $deleteRefused=$true }
        Assert $deleteRefused 'Fixture rollback file is not genuinely locked'
        $output=(Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} 6>&1 | Out-String -Width 4096)
        Assert ($output -match 'in use.*next time') 'Locked rollback skip message missing'
        Assert (Test-Path -LiteralPath $lockedPath) 'Locked rollback removed'
        Assert (-not (Test-Path -LiteralPath (Join-Path $scenario.Install ('quota-baton-tray.exe.old-'+$oldest)))) 'Deletable mate was not removed'
        Assert (@(Get-ChildItem -LiteralPath $scenario.Install -File | Where-Object { $_.Name.EndsWith($middle) }).Count -eq 0) 'Other deletable generation retained'
        $remaining=@(Get-ChildItem -LiteralPath $scenario.Install -File | Where-Object { $_.Name -match '\.old-\d{21}$' -and $_.FullName -ne $lockedPath })
        Assert ($remaining.Count -eq 2) 'Newest rollback pair not retained'
        Assert (($remaining[0].Name -split '\.old-')[1] -eq ($remaining[1].Name -split '\.old-')[1]) 'Retained pair generations differ'
        Assert ($scenario.Events.Contains('cli:status')) 'Retention prevented final setup status'
        Assert ($env:PATH -ceq $scenario.InputPath) 'Parent process PATH was not restored after wire'
        Assert ($output -match [regex]::Escape($realTargets.codex) -and $output -match [regex]::Escape($realTargets.claude)) 'Detection did not report real targets'
        Assert ($output -match 'herdr') 'Wrapper annotation missing'
        Assert (@($scenario.Processes | Where-Object { $_.ExecutablePath -eq $scenario.Tray }).Count -eq 1) 'Upgrade left no tray'
        $locked.Dispose(); $script:locked=$null
        Invoke-FixtureScript $uninstaller @{InstallDir=$scenario.Install;Lang='en'}
        Assert (-not (Test-Path -LiteralPath $scenario.Install)) 'Mock uninstall did not remove fixture install'
        Assert (Test-Path -LiteralPath (Join-Path $env:QB_HOME 'keep.txt')) 'Uninstall removed kept account data'
        Assert ($scenario.Events.Contains('cli:unwire --apply')) 'Unwire was not requested'
        Assert (@($scenario.Processes | Where-Object { $_.ProcessId -eq 999 }).Count -eq 1) 'Unrelated process changed'
    }
    Case 'uninstall-under-unowned-lock-refuses-without-false-complete-or-data-loss' {
        $scenario=New-Scenario 'locked-uninstall'
        $lockedPath=Join-Path $scenario.Install 'quota-baton.exe.old-200001010000000000000'
        [IO.File]::WriteAllText($lockedPath,'unowned locked rollback')
        $script:locked=[IO.File]::Open($lockedPath,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::None)
        $messages=New-Object 'System.Collections.Generic.List[string]'
        $failed=$false
        try {
            Invoke-FixtureScript $uninstaller @{InstallDir=$scenario.Install;Lang='en'} 6>&1 3>&1 |
                ForEach-Object { $messages.Add($_.ToString()) }
        } catch { $failed=$true; $messages.Add($_.Exception.Message) }
        Assert $failed 'Locked uninstall unexpectedly reported success'
        Assert (Test-Path -LiteralPath $lockedPath) 'Locked residual file disappeared'
        Assert (($messages -join "`n") -notmatch 'Uninstall complete\.') 'Locked uninstall falsely reported complete'
        Assert (Test-Path -LiteralPath (Join-Path $env:QB_HOME 'keep.txt')) 'Locked uninstall removed account data'
        Assert (@($scenario.Processes | Where-Object { $_.ProcessId -eq 999 }).Count -eq 1) 'Locked uninstall touched unrelated process'
        $locked.Dispose(); $script:locked=$null
        Write-Host 'Expected locked uninstall refusal recorded; no partial-install retry attempted.'
    }
    Case 'late-setup-failure-finally-restores-tray' {
        $scenario=New-Scenario 'late-failure' 'late'
        $failed=$false
        try { Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} } catch { $failed=$_.Exception.Message -match 'synthetic late setup failure' }
        Assert $failed 'Late failure injection did not execute'
        Assert ($scenario.Events.Contains('stop:quota-baton-tray.exe')) 'Late failure did not pass old tray stop'
        Assert (@($scenario.Processes | Where-Object { $_.ExecutablePath -eq $scenario.Tray }).Count -ge 1) 'Finally left no tray after late failure'
    }
    Case 'swap-failure-preserves-or-restores-old-tray' {
        $scenario=New-Scenario 'swap-failure' 'swap'
        $failed=$false
        try { Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} } catch { $failed=$_.Exception.Message -match 'synthetic tray copy failure' }
        Assert $failed 'Swap failure injection did not execute'
        Assert (([IO.File]::ReadAllText($scenario.Tray)) -eq 'old:quota-baton-tray.exe') 'Old tray binary not restored'
        Assert (@($scenario.Processes | Where-Object { $_.ExecutablePath -eq $scenario.Tray }).Count -ge 1) 'Swap failure left no old tray'
    }
    Case 'wire-failure-restores-parent-path-with-native-claude-not-on-original-path' {
        $scenario=New-Scenario 'wire-failure' 'wire'
        Assert (-not ($env:PATH -split ';' -contains $nativeBin)) 'Native Claude parent unexpectedly present before wire'
        $failed=$false
        try { Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} } catch { $failed=$_.Exception.Message -match 'synthetic wire apply failure' }
        Assert $failed 'Wire failure injection did not execute'
        Assert ($env:PATH -ceq $scenario.InputPath) 'Wire failure leaked process PATH change'
        Assert (@($scenario.Processes | Where-Object { $_.ExecutablePath -eq $scenario.Tray }).Count -ge 1) 'Wire failure left no tray'
    }
    Case 'full-plain-iex-null-pscmdlet-install-and-uninstall' {
        $scenario=New-Scenario 'plain-iex'
        Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} -PlainIex
        Assert ($scenario.Events.Contains('plain-iex-null')) 'IEX null PSCmdlet context not verified'
        Assert ($scenario.Events.Contains('cli:status')) 'Plain IEX installer did not complete full mock setup'
        Assert (([IO.File]::ReadAllText($scenario.Exe)) -eq 'new:quota-baton.exe') 'Plain IEX did not install fixture payload'
        Invoke-FixtureScript $uninstaller @{InstallDir=$scenario.Install;RemoveData=$true;Lang='en'} -PlainIex
        Assert (@($scenario.Events | Where-Object { $_ -eq 'plain-iex-null' }).Count -eq 2) 'Uninstall did not execute with null PSCmdlet'
        Assert ($scenario.Events.Contains('cli:unwire --apply')) 'Plain IEX did not request mocked unwire'
        Assert (-not (Test-Path -LiteralPath $scenario.Install)) 'Plain IEX uninstall did not remove synthetic install'
        Assert (-not (Test-Path -LiteralPath $env:QB_HOME)) 'Plain IEX RemoveData null guard did not remove synthetic data'
        [void][IO.Directory]::CreateDirectory($env:QB_HOME)
        [IO.File]::WriteAllText((Join-Path $env:QB_HOME 'keep.txt'),'synthetic-account-data')
    }
    Case 'plain-iex-null-pscmdlet-whatif-has-no-install-uninstall-or-data-effects' {
        $scenario=New-Scenario 'plain-iex-whatif'
        Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} -PlainIex -IexWhatIf $true
        Invoke-FixtureScript $uninstaller @{InstallDir=$scenario.Install;RemoveData=$true;Lang='en'} -PlainIex -IexWhatIf $true
        Assert (@($scenario.Events | Where-Object { $_ -eq 'plain-iex-null' }).Count -eq 2) 'WhatIf null context not verified for both scripts'
        Assert (@($scenario.Events | Where-Object { $_ -match '^(cli:|start:|stop:|user-path)' }).Count -eq 0) 'Null PSCmdlet WhatIf caused workflow side effects'
        Assert (([IO.File]::ReadAllText($scenario.Exe)) -eq 'old:quota-baton.exe') 'WhatIf changed fixture core'
        Assert (([IO.File]::ReadAllText($scenario.Tray)) -eq 'old:quota-baton-tray.exe') 'WhatIf changed fixture tray'
        Assert (Test-Path -LiteralPath (Join-Path $env:QB_HOME 'keep.txt')) 'WhatIf removed synthetic account data'
    }
    foreach ($extensionCase in @('absent','empty','no-exe','reordered')) {
        Case ('wire-environment-restore-' + $extensionCase) {
            if ($extensionCase -eq 'absent') { Remove-Item Env:PATHEXT -ErrorAction SilentlyContinue }
            elseif ($extensionCase -eq 'empty') { $env:PATHEXT='' }
            elseif ($extensionCase -eq 'no-exe') { $env:PATHEXT='.COM;.CMD;.BAT' }
            else { $env:PATHEXT='.COM;.CMD;.EXE;.BAT' }
            $scenario=New-Scenario ('extensions-'+$extensionCase) 'wire'
            $failed=$false
            try { Invoke-FixtureScript $installer @{ZipPath=$zip;InstallDir=$scenario.Install;Yes=$true;Lang='en'} } catch { $failed=$_.Exception.Message -match 'synthetic wire apply failure' }
            Assert $failed 'Wire environment failure injection did not execute or real target resolution failed'
            Assert ($env:PATH -ceq $scenario.InputPath) 'PATH not restored exactly after wire failure'
            Assert ($env:PATHEXT -ceq $scenario.InputPathExt) 'PATHEXT not restored exactly after wire failure'
            Assert ((Test-Path Env:PATHEXT) -eq $scenario.InputPathExtPresent) 'PATHEXT presence not restored'
        }
    }
} finally {
    if ($locked) { $locked.Dispose() }
    foreach ($junction in $junctions) { [void](Contained $junction); [IO.Directory]::Delete($junction) }
    [pscustomobject]@{
        runtime=$PSVersionTable.PSVersion.ToString(); installer_sha256=(Get-FileHash -LiteralPath $installer).Hash;
        uninstaller_sha256=(Get-FileHash -LiteralPath $uninstaller).Hash; harness_sha256=(Get-FileHash -LiteralPath $PSCommandPath).Hash;
        cases=@($receipts.ToArray()); limitations='Real filesystem and locked file; native CLI invocation, health, user PATH and deterministic copy failure are AST mocked. Actual RunCli process PATH/PATHEXT scope retained. Nonexecuting Claude finder checks reviewed extension ordering; no Rust wire or generated launcher execution. Fake process table; no live process/provider operations.'
    } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $EvidenceDir 'result.json') -Encoding UTF8
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name,$saved[$name],'Process') }
    Stop-Transcript | Out-Null
    $sandbox=[IO.Path]::GetFullPath((Join-Path $project '.sandbox')).TrimEnd('\')+'\'
    Assert (([IO.Path]::GetFullPath($fixture)).StartsWith($sandbox,[StringComparison]::OrdinalIgnoreCase)) 'Unsafe fixture cleanup'
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
