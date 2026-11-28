# Installs, inspects, starts, stops or removes the dedicated GLPH
# Session-0 Windows Service (GLPHService / glph-service.exe).
# Windows PowerShell 5.1.
#requires -Version 5.1
param(
    [ValidateSet('Install', 'Inspect', 'Start', 'Stop', 'Remove')]
    [string]$Mode = 'Install',
    [string]$SourceBinary = '',
    [ValidateSet('Automatic', 'Manual')]
    [string]$StartType = 'Automatic'
)
$ErrorActionPreference = 'Stop'

$svcName = 'GLPHService'
$svcDisplayName = 'GLPH Hardware & Cooling Service'
$svcDescription = 'Direct hardware cooling and zero-stutter sensor background service for GamingLaptopPerformanceHub (GLPH).'
$svcRoot = [IO.Path]::GetFullPath((Join-Path $env:ProgramFiles 'GLPH'))
$svcBinary = Join-Path $svcRoot 'glph-service.exe'
$svcReceipt = Join-Path $svcRoot 'service-receipt.json'
$svcDataRoot = [IO.Path]::GetFullPath((Join-Path $env:ProgramData 'GLPH'))
$svcExisting = Get-Service -Name $svcName -ErrorAction SilentlyContinue

if ($Mode -eq 'Inspect') {
    $wmiSvc = Get-CimInstance -ClassName Win32_Service -Filter ("Name='" + $svcName + "'") -ErrorAction SilentlyContinue
    [pscustomobject]@{
        Installed  = ($null -ne $svcExisting)
        Name       = $svcName
        Status     = if ($null -ne $svcExisting) { [string]$svcExisting.Status } else { 'NotInstalled' }
        StartMode  = if ($null -ne $wmiSvc) { [string]$wmiSvc.StartMode } else { $null }
        PathName   = if ($null -ne $wmiSvc) { [string]$wmiSvc.PathName } else { $null }
        BinaryHash = if (Test-Path -LiteralPath $svcBinary) { (Get-FileHash -LiteralPath $svcBinary -Algorithm SHA256).Hash } else { $null }
    } | ConvertTo-Json
    return
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]::new($identity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run scripts/install-service.ps1 elevated (Administrator) to manage GLPHService.'
}

function Assert-NotReparsePoint {
    param([string]$Path)
    if ((Test-Path -LiteralPath $Path) -and ((Get-Item -LiteralPath $Path).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw ("Refusing redirected path (reparse point): " + $Path)
    }
}

function Wait-ProcessImageReleased {
    param([string]$BinaryPath, [int]$TimeoutSeconds = 8)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $running = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
            try { $_.Path -and [IO.Path]::GetFullPath($_.Path).Equals($BinaryPath, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
        })
        if ($running.Count -eq 0) { return }
        Start-Sleep -Milliseconds 150
    }
    throw ("Service binary is still locked by a running process: " + $BinaryPath)
}

function Copy-FileWithRetry {
    param([string]$Source, [string]$Destination, [int]$Attempts = 10)
    for ($i = 1; $i -le $Attempts; $i++) {
        try {
            Copy-Item -LiteralPath $Source -Destination $Destination -Force
            return
        } catch {
            if ($i -eq $Attempts) { throw }
            Start-Sleep -Milliseconds 200
        }
    }
}

Assert-NotReparsePoint $svcRoot
Assert-NotReparsePoint $svcBinary
Assert-NotReparsePoint $svcReceipt
Assert-NotReparsePoint $svcDataRoot

if ($Mode -eq 'Start') {
    if ($null -eq $svcExisting) { throw 'GLPHService is not installed.' }
    if ($svcExisting.Status -ne 'Running') {
        Start-Service -Name $svcName
        $svcExisting.WaitForStatus('Running', [TimeSpan]::FromSeconds(15))
    }
    [pscustomobject]@{ Started = $true; Name = $svcName; Status = [string](Get-Service -Name $svcName).Status } | ConvertTo-Json
    return
}

if ($Mode -eq 'Stop') {
    if ($null -eq $svcExisting) { throw 'GLPHService is not installed.' }
    if ($svcExisting.Status -ne 'Stopped') {
        Stop-Service -Name $svcName -Force
        $svcExisting.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
    }
    [pscustomobject]@{ Stopped = $true; Name = $svcName; Status = [string](Get-Service -Name $svcName).Status } | ConvertTo-Json
    return
}

if ($Mode -eq 'Remove') {
    if ($null -ne $svcExisting) {
        if ($svcExisting.Status -ne 'Stopped') {
            Stop-Service -Name $svcName -Force -ErrorAction SilentlyContinue
            try { $svcExisting.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15)) } catch {}
        }
        Wait-ProcessImageReleased -BinaryPath $svcBinary
        $scExe = Join-Path $env:SystemRoot 'System32\sc.exe'
        $delProc = Start-Process -FilePath $scExe -ArgumentList ('delete ' + $svcName) -Wait -NoNewWindow -PassThru
        if ($delProc.ExitCode -ne 0 -and $delProc.ExitCode -ne 1060) {
            throw ("sc.exe delete failed with exit code " + $delProc.ExitCode)
        }
        $deadline = (Get-Date).AddSeconds(10)
        while ((Get-Date) -lt $deadline -and (Get-Service -Name $svcName -ErrorAction SilentlyContinue)) {
            Start-Sleep -Milliseconds 200
        }
    }
    foreach ($p in @($svcReceipt, $svcBinary)) {
        if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force }
    }
    [pscustomobject]@{ Removed = $true; Name = $svcName } | ConvertTo-Json
    return
}

# Mode -eq 'Install'
$source = if ([string]::IsNullOrWhiteSpace($SourceBinary)) {
    if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
        throw 'SourceBinary must be specified when install-service.ps1 runs inline.'
    }
    $repoRoot = Split-Path -Parent $PSScriptRoot
    $releaseCandidate = Join-Path $repoRoot 'target\release\glph-service.exe'
    $distCandidate = Join-Path $repoRoot 'dist\glph-service.exe'
    if (Test-Path -LiteralPath $releaseCandidate -PathType Leaf) { $releaseCandidate } else { $distCandidate }
} else {
    [IO.Path]::GetFullPath($SourceBinary)
}
if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
    throw ("Service executable not found at: " + $source + ". Build with 'cargo build --release' first.")
}
$sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash

# Never stop legacy cooling tasks or replace service binaries while an explicit
# HPOmenCap qualification/recovery session holds the ownership gate.
foreach ($recoveryFile in @('controller-ownership.json','controller-cooling.json')) {
    $recoveryPath = Join-Path $svcRoot $recoveryFile
    if (Test-Path -LiteralPath $recoveryPath) {
        $recoveryItem = Get-Item -LiteralPath $recoveryPath
        if ($recoveryItem.Length -gt 16384 -or ($recoveryItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'Controller recovery record is oversized or redirected; service install was blocked.'
        }
        $recoveryRecord = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json
        if ($recoveryRecord.schema -ne 1 -or $recoveryRecord.phase -cnotin @('restored','superseded') -or $recoveryRecord.owner.session -cnotmatch '^[a-fA-F0-9]{64}$') {
            throw 'Complete controller/cooling recovery before modifying GLPHService.'
        }
    }
}

if ($null -ne $svcExisting -and $svcExisting.Status -ne 'Stopped') {
    Stop-Service -Name $svcName -Force
    $svcExisting.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
}
Wait-ProcessImageReleased -BinaryPath $svcBinary

# Stop and disable any legacy per-user Session-1 fan-service logon tasks so
# GLPHService (Session 0) is the sole cooling authority.
try {
    $legacyTasks = @(Get-ScheduledTask -TaskName 'GLPH-Fan-*' -ErrorAction SilentlyContinue) +
                   @(Get-ScheduledTask -TaskName 'PerformanceHub-Fan-*' -ErrorAction SilentlyContinue)
    foreach ($t in $legacyTasks) {
        if ($t.State -eq 'Running') {
            Stop-ScheduledTask -TaskName $t.TaskName -ErrorAction SilentlyContinue
        }
        Disable-ScheduledTask -TaskName $t.TaskName -ErrorAction SilentlyContinue | Out-Null
    }
} catch {}

New-Item -ItemType Directory -Force -Path $svcRoot | Out-Null
New-Item -ItemType Directory -Force -Path $svcDataRoot | Out-Null
Assert-NotReparsePoint $svcRoot
Assert-NotReparsePoint $svcDataRoot

# Protect C:\Program Files\GLPH: Administrators + SYSTEM FullControl, Users ReadAndExecute.
$admins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
$system = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
$users = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
$inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit'
$prop = [Security.AccessControl.PropagationFlags]::None

$dirAcl = [Security.AccessControl.DirectorySecurity]::new()
$dirAcl.SetAccessRuleProtection($true, $false)
$dirAcl.SetOwner($admins)
foreach ($principal in @($admins, $system)) {
    $dirAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($principal, [Security.AccessControl.FileSystemRights]::FullControl, $inherit, $prop, [Security.AccessControl.AccessControlType]::Allow))
}
$dirAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($users, [Security.AccessControl.FileSystemRights]::ReadAndExecute, $inherit, $prop, [Security.AccessControl.AccessControlType]::Allow))
Set-Acl -LiteralPath $svcRoot -AclObject $dirAcl

# Protect C:\ProgramData\GLPH: Admins + SYSTEM FullControl, Users ReadAndExecute.
# Live configuration changes go through the validated Named Pipe in LocalSystem.
$dataAcl = [Security.AccessControl.DirectorySecurity]::new()
$dataAcl.SetAccessRuleProtection($true, $false)
$dataAcl.SetOwner($admins)
foreach ($principal in @($admins, $system)) {
    $dataAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($principal, [Security.AccessControl.FileSystemRights]::FullControl, $inherit, $prop, [Security.AccessControl.AccessControlType]::Allow))
}
$dataAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($users, [Security.AccessControl.FileSystemRights]::ReadAndExecute, $inherit, $prop, [Security.AccessControl.AccessControlType]::Allow))
Set-Acl -LiteralPath $svcDataRoot -AclObject $dataAcl

if (-not $source.Equals($svcBinary, [StringComparison]::OrdinalIgnoreCase)) {
    Copy-FileWithRetry -Source $source -Destination $svcBinary
}
if ((Get-FileHash -LiteralPath $svcBinary -Algorithm SHA256).Hash -ne $sourceHash) {
    throw 'Installed service binary SHA-256 hash differs from source.'
}
$fileAcl = [Security.AccessControl.FileSecurity]::new()
$fileAcl.SetAccessRuleProtection($false, $false)
$fileAcl.SetOwner($admins)
Set-Acl -LiteralPath $svcBinary -AclObject $fileAcl

$scStart = if ($StartType -eq 'Automatic') { 'auto' } else { 'demand' }
$quotedBin = '"' + $svcBinary + '"'
$scExe = Join-Path $env:SystemRoot 'System32\sc.exe'
if ($null -eq $svcExisting) {
    New-Service -Name $svcName -BinaryPathName $quotedBin -DisplayName $svcDisplayName -Description $svcDescription -StartupType $StartType | Out-Null
} else {
    $cfgProc = Start-Process -FilePath $scExe -ArgumentList ('config ' + $svcName + ' binPath= "\"' + $svcBinary + '\"" start= ' + $scStart + ' DisplayName= "' + $svcDisplayName + '" obj= LocalSystem') -Wait -NoNewWindow -PassThru
    if ($cfgProc.ExitCode -ne 0) { throw ("sc.exe config failed with exit code " + $cfgProc.ExitCode) }
    $descProc = Start-Process -FilePath $scExe -ArgumentList ('description ' + $svcName + ' "' + $svcDescription + '"') -Wait -NoNewWindow -PassThru
    if ($descProc.ExitCode -ne 0) { throw ("sc.exe description failed with exit code " + $descProc.ExitCode) }
}
# Automatic SCM failure recovery: restart after 5s, 15s, 30s; reset failure count after 24h.
$failProc = Start-Process -FilePath $scExe -ArgumentList ('failure ' + $svcName + ' reset= 86400 actions= restart/5000/restart/15000/restart/30000') -Wait -NoNewWindow -PassThru
if ($failProc.ExitCode -ne 0) { throw ("sc.exe failure failed with exit code " + $failProc.ExitCode) }

[ordered]@{
    schema       = 1
    service_name = $svcName
    executable   = $svcBinary
    sha256       = $sourceHash
    start_type   = $StartType
    installed_at = (Get-Date).ToString('o')
} | ConvertTo-Json | Set-Content -LiteralPath $svcReceipt -Encoding UTF8

Start-Service -Name $svcName
$svc = Get-Service -Name $svcName
$svc.WaitForStatus('Running', [TimeSpan]::FromSeconds(15))

[pscustomobject]@{
    Installed  = $true
    Name       = $svcName
    StartType  = $StartType
    Status     = [string](Get-Service -Name $svcName).Status
    SHA256     = $sourceHash
} | ConvertTo-Json
