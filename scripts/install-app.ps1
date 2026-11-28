# Installs the GLPH release executable (and sibling glph-service.exe plus
# shipped .glph plugins) into either C:\Program Files\GLPH (Scope=Machine, UAC)
# or %LOCALAPPDATA%\Programs\GLPH (Scope=User, no UAC) and creates Start menu
# and desktop shortcuts with the embedded icon.
# No profile is selected, no logon task is registered and no hardware is touched.
# Windows PowerShell 5.1.
#requires -Version 5.1
param(
    [ValidateSet('Install','Inspect','Remove')][string]$Mode = 'Install',
    [ValidateSet('Machine','User')][string]$Scope = 'Machine',
    [string]$SourceBinary = '',
    [switch]$NoDesktopShortcut,
    # Internal: keeps the self-elevated window readable when it fails.
    [switch]$PauseOnError
)
$ErrorActionPreference = 'Stop'
trap {
    if ($PauseOnError) {
        Write-Host $_ -ForegroundColor Red
        [void](Read-Host 'Press Enter to close')
    }
    break
}
$appName = 'GLPH'
$appRoot = if ($Scope -eq 'User') {
    [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\GLPH'))
} else {
    [IO.Path]::GetFullPath((Join-Path $env:ProgramFiles 'GLPH'))
}
$appBinary = Join-Path $appRoot 'glph.exe'
$appServiceBinary = Join-Path $appRoot 'glph-service.exe'
$appStartupReceipt = Join-Path $appRoot 'installation.json'
$appShortcuts = @(
    (Join-Path ([Environment]::GetFolderPath('Programs')) "$appName.lnk"),
    (Join-Path ([Environment]::GetFolderPath('Desktop')) "$appName.lnk")
)
$appIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$appTaskName = 'GLPH-Profile-' + $appIdentity.User.Value

function Get-GlphShortcut {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $link = (New-Object -ComObject 'WScript.Shell').CreateShortcut($Path)
    [pscustomobject]@{ Path = $Path; Target = $link.TargetPath; Icon = $link.IconLocation }
}

if ($Mode -eq 'Inspect') {
    [pscustomobject]@{
        Installed = (Test-Path -LiteralPath $appBinary -PathType Leaf)
        Scope = $Scope
        Executable = $appBinary
        ServiceBinary = if (Test-Path -LiteralPath $appServiceBinary -PathType Leaf) { $appServiceBinary } else { $null }
        SHA256 = if (Test-Path -LiteralPath $appBinary) { (Get-FileHash -LiteralPath $appBinary -Algorithm SHA256).Hash } else { $null }
        Shortcuts = @($appShortcuts | ForEach-Object { Get-GlphShortcut $_ } | Where-Object { $null -ne $_ })
        LogonTask = ($null -ne (Get-ScheduledTask -TaskName $appTaskName -ErrorAction SilentlyContinue))
    } | ConvertTo-Json -Depth 3
    return
}

$repoRoot = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { Split-Path -Parent $PSScriptRoot } else { $null }
$appSource = if (-not [string]::IsNullOrWhiteSpace($SourceBinary)) {
    [IO.Path]::GetFullPath($SourceBinary)
} elseif ($Mode -eq 'Remove') {
    ''
} else {
    if ([string]::IsNullOrWhiteSpace($PSScriptRoot)) {
        throw 'SourceBinary must be specified when install-app.ps1 runs inline.'
    }
    if (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'glph.exe') -PathType Leaf) {
        Join-Path $PSScriptRoot 'glph.exe'
    } elseif (Test-Path -LiteralPath (Join-Path $repoRoot 'dist\glph.exe') -PathType Leaf) {
        Join-Path $repoRoot 'dist\glph.exe'
    } else {
        Join-Path $repoRoot 'target\release\glph.exe'
    }
}
$appServiceSource = if (-not [string]::IsNullOrWhiteSpace($appSource)) { Join-Path (Split-Path -Parent $appSource) 'glph-service.exe' } else { '' }
$appPluginsSourceDir = if (-not [string]::IsNullOrWhiteSpace($repoRoot)) { Join-Path $repoRoot 'plugins' } else { '' }

if ($Scope -eq 'Machine' -and -not ([Security.Principal.WindowsPrincipal]::new($appIdentity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    if ([string]::IsNullOrWhiteSpace($PSCommandPath)) {
        throw 'Run install-app.ps1 from a script file or elevated Administrator shell for Machine scope.'
    }
    $appArguments = @('-NoProfile','-ExecutionPolicy','Bypass','-File',('"' + $PSCommandPath + '"'),'-Mode',$Mode,'-Scope',$Scope,'-PauseOnError')
    if ($Mode -eq 'Install') { $appArguments += @('-SourceBinary',('"' + $appSource + '"')) }
    if ($NoDesktopShortcut) { $appArguments += '-NoDesktopShortcut' }
    $appProcess = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList $appArguments -Verb RunAs -Wait -PassThru
    if ($appProcess.ExitCode -ne 0) { throw "Elevated $Mode failed with exit code $($appProcess.ExitCode)." }
    & $PSCommandPath -Mode Inspect -Scope $Scope
    return
}

# Serialize against an active GLPH controller session before any installer action.
$recoveryGate = [Threading.Mutex]::new($false, 'Global\GLPH.HpControllerOwner')
$recoveryGateOwned = $false
try {
    try { $recoveryGateOwned = $recoveryGate.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $recoveryGateOwned = $true }
    if (-not $recoveryGateOwned) { throw 'A GLPH controller session is active; installation/removal was blocked.' }
    foreach ($recoveryFile in @('controller-ownership.json','controller-cooling.json')) {
        $recoveryPath = Join-Path $appRoot $recoveryFile
        if (Test-Path -LiteralPath $recoveryPath) {
            $recoveryItem = Get-Item -LiteralPath $recoveryPath
            if ($recoveryItem.Length -gt 16384 -or ($recoveryItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Controller recovery record is oversized or redirected; installation/removal was blocked.' }
            $recoveryRecord = Get-Content -LiteralPath $recoveryPath -Raw | ConvertFrom-Json
            if ($recoveryRecord.schema -ne 1 -or $recoveryRecord.phase -cnotin @('restored','superseded') -or $recoveryRecord.owner.session -cnotmatch '^[a-fA-F0-9]{64}$') {
                throw 'Complete controller/cooling recovery before updating or removing either helper.'
            }
        }
    }
    if ((Test-Path -LiteralPath $appRoot) -and ((Get-Item -LiteralPath $appRoot).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'The installation directory is a reparse point; nothing was changed.'
    }
    foreach ($checkedFile in @($appBinary, $appServiceBinary)) {
        if ((Test-Path -LiteralPath $checkedFile) -and ((Get-Item -LiteralPath $checkedFile).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'An installed executable is a reparse point; nothing was changed.'
        }
    }
    $appRunning = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
        try { $_.Path -and [IO.Path]::GetFullPath($_.Path).Equals($appBinary, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
    })
    if ($appRunning.Count -gt 0) {
        throw 'GLPH is running from the installation directory. Exit it from the tray menu first.'
    }

    if ($Mode -eq 'Remove') {
        $appSvcInstalled = ($Scope -eq 'Machine') -and ($null -ne (Get-Service -Name 'GLPHService' -ErrorAction SilentlyContinue))
        foreach ($appShortcut in $appShortcuts) {
            $appLink = Get-GlphShortcut $appShortcut
            if ($null -ne $appLink -and $appLink.Target -eq $appBinary) { Remove-Item -LiteralPath $appShortcut }
        }
        $appTask = $null -ne (Get-ScheduledTask -TaskName $appTaskName -ErrorAction SilentlyContinue)
        if ($appTask -or (Test-Path -LiteralPath $appStartupReceipt)) {
            [pscustomobject]@{ ShortcutsRemoved = $true; ExecutableRemoved = $false; Reason = 'Logon task installed; run scripts/install-startup.ps1 -Mode Remove first.'; ProfilesRetained = $true } | ConvertTo-Json
            return
        }
        if (Test-Path -LiteralPath $appBinary) { Remove-Item -LiteralPath $appBinary }
        if ($appSvcInstalled) {
            [pscustomobject]@{ ShortcutsRemoved = $true; ExecutableRemoved = $true; ServiceBinaryRemoved = $false; Reason = 'GLPHService is installed; run scripts/install-service.ps1 -Mode Remove to remove the service.'; ProfilesRetained = $true } | ConvertTo-Json
            return
        }
        if (Test-Path -LiteralPath $appServiceBinary) { Remove-Item -LiteralPath $appServiceBinary }
        [pscustomobject]@{ ShortcutsRemoved = $true; ExecutableRemoved = $true; ProfilesRetained = $true } | ConvertTo-Json
        return
    }

    if (-not (Test-Path -LiteralPath $appSource -PathType Leaf)) { throw "Release executable not found at $appSource. Build with cargo build --release --workspace first." }
    $appSourceHash = (Get-FileHash -LiteralPath $appSource -Algorithm SHA256).Hash
    $appPreviousHash = if (Test-Path -LiteralPath $appBinary) { (Get-FileHash -LiteralPath $appBinary -Algorithm SHA256).Hash } else { $null }

    New-Item -ItemType Directory -Force -Path $appRoot | Out-Null
    if ($Scope -eq 'Machine') {
        $appAcl = [Security.AccessControl.DirectorySecurity]::new()
        $appAcl.SetAccessRuleProtection($true, $false)
        $appAdmins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
        $appSystem = [Security.Principal.SecurityIdentifier]::new('S-1-5-18')
        $appUsers = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
        $appAcl.SetOwner($appAdmins)
        $appInheritance = [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit'
        $appPropagation = [Security.AccessControl.PropagationFlags]::None
        foreach ($appPrincipal in @($appAdmins, $appSystem)) {
            $appAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($appPrincipal, [Security.AccessControl.FileSystemRights]::FullControl, $appInheritance, $appPropagation, [Security.AccessControl.AccessControlType]::Allow))
        }
        $appAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($appUsers, [Security.AccessControl.FileSystemRights]::ReadAndExecute, $appInheritance, $appPropagation, [Security.AccessControl.AccessControlType]::Allow))
        Set-Acl -LiteralPath $appRoot -AclObject $appAcl
    }

    if (-not $appSource.Equals($appBinary, [StringComparison]::OrdinalIgnoreCase)) {
        Copy-Item -LiteralPath $appSource -Destination $appBinary -Force
    }
    if ((Get-FileHash -LiteralPath $appBinary -Algorithm SHA256).Hash -ne $appSourceHash) { throw 'Installed executable hash differs from the release.' }

    $serviceInstalled = $false
    if (-not [string]::IsNullOrWhiteSpace($appServiceSource) -and (Test-Path -LiteralPath $appServiceSource -PathType Leaf)) {
        if (-not $appServiceSource.Equals($appServiceBinary, [StringComparison]::OrdinalIgnoreCase)) {
            $svc = if ($Scope -eq 'Machine') { Get-Service -Name 'GLPHService' -ErrorAction SilentlyContinue } else { $null }
            $svcWasRunning = ($null -ne $svc -and $svc.Status -ne 'Stopped')
            if ($svcWasRunning) {
                Stop-Service -Name 'GLPHService' -Force
                $svc.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(15))
                $svcDeadline = (Get-Date).AddSeconds(8)
                while ((Get-Date) -lt $svcDeadline) {
                    $svcProcs = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
                        try { $_.Path -and [IO.Path]::GetFullPath($_.Path).Equals($appServiceBinary, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
                    })
                    if ($svcProcs.Count -eq 0) { break }
                    Start-Sleep -Milliseconds 150
                }
            }
            try {
                for ($attempt = 1; $attempt -le 10; $attempt++) {
                    try {
                        Copy-Item -LiteralPath $appServiceSource -Destination $appServiceBinary -Force
                        break
                    } catch {
                        if ($attempt -eq 10) { throw }
                        Start-Sleep -Milliseconds 200
                    }
                }
                $svcReceipt = Join-Path $appRoot 'service-receipt.json'
                if ($Scope -eq 'Machine' -and (Test-Path -LiteralPath $svcReceipt -PathType Leaf)) {
                    $svcHash = (Get-FileHash -LiteralPath $appServiceBinary -Algorithm SHA256).Hash
                    $receiptObj = Get-Content -LiteralPath $svcReceipt -Raw | ConvertFrom-Json
                    $receiptObj.sha256 = $svcHash
                    $receiptObj.installed_at = (Get-Date).ToString('o')
                    $receiptObj | ConvertTo-Json | Set-Content -LiteralPath $svcReceipt -Encoding UTF8
                }
            } finally {
                if ($svcWasRunning) {
                    Start-Service -Name 'GLPHService'
                    $svc.WaitForStatus('Running', [TimeSpan]::FromSeconds(15))
                }
            }
        }
        $serviceInstalled = $true
    }

    $pluginsInstalled = @()
    if (-not [string]::IsNullOrWhiteSpace($appPluginsSourceDir) -and (Test-Path -LiteralPath $appPluginsSourceDir -PathType Container)) {
        $appPluginsDestDir = Join-Path $appRoot 'plugins'
        New-Item -ItemType Directory -Force -Path $appPluginsDestDir | Out-Null
        foreach ($pluginFile in @(Get-ChildItem -LiteralPath $appPluginsSourceDir -Filter '*.glph' -File -ErrorAction SilentlyContinue)) {
            $destPlugin = Join-Path $appPluginsDestDir $pluginFile.Name
            Copy-Item -LiteralPath $pluginFile.FullName -Destination $destPlugin -Force
            $pluginsInstalled += $destPlugin
        }
    }

    if ($Scope -eq 'Machine') {
        $appAdmins = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544')
        foreach ($binPath in @($appBinary, $appServiceBinary)) {
            if (Test-Path -LiteralPath $binPath -PathType Leaf) {
                $appFileAcl = [Security.AccessControl.FileSecurity]::new()
                $appFileAcl.SetAccessRuleProtection($false, $false)
                $appFileAcl.SetOwner($appAdmins)
                Set-Acl -LiteralPath $binPath -AclObject $appFileAcl
            }
        }
    }

    $appShell = New-Object -ComObject 'WScript.Shell'
    $appCreated = @()
    foreach ($appShortcut in $(if ($NoDesktopShortcut) { @($appShortcuts[0]) } else { $appShortcuts })) {
        $appLink = $appShell.CreateShortcut($appShortcut)
        $appLink.TargetPath = $appBinary
        $appLink.WorkingDirectory = $appRoot
        $appLink.IconLocation = "$appBinary,0"
        $appLink.Description = "GamingLaptopPerformanceHub - GPU/CPU-Telemetrie, Presets, L$([char]0x00FC)fter und Plugins"
        $appLink.Save()
        $appCreated += $appShortcut
    }
    if ($NoDesktopShortcut -and (Test-Path -LiteralPath $appShortcuts[1])) {
        $appLink = $appShell.CreateShortcut($appShortcuts[1])
        $appLink.TargetPath = $appBinary
        $appLink.WorkingDirectory = $appRoot
        $appLink.IconLocation = "$appBinary,0"
        $appLink.Save()
        $appCreated += $appShortcuts[1]
    }

    [pscustomobject]@{
        Installed = $true
        Scope = $Scope
        Executable = $appBinary
        ServiceInstalled = $serviceInstalled
        ServiceBinary = if ($serviceInstalled) { $appServiceBinary } else { $null }
        PluginsCopied = $pluginsInstalled
        SHA256 = $appSourceHash
        Shortcuts = $appCreated
        StartupApprovalInvalidated = ($null -ne $appPreviousHash -and $appPreviousHash -ne $appSourceHash -and (Test-Path -LiteralPath $appStartupReceipt))
        ProfileSelected = $false
        HardwareApplied = $false
    } | ConvertTo-Json -Depth 3
} finally {
    if ($recoveryGateOwned) { $recoveryGate.ReleaseMutex() }
    $recoveryGate.Dispose()
}
