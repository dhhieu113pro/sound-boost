<#
.SYNOPSIS
    First-time install of the Sound Booster APO on this machine.

.DESCRIPTION
    Run this from an *elevated* PowerShell -- the endpoint effect chain, the APO
    store and the audio service all need it:

        powershell -ExecutionPolicy Bypass -File tools\first-install.ps1

    It uninstalls Letasoft Sound Booster first: that product's endpoint effect
    occupies the same FxProperties slot we append to, so leaving it installed
    would process the audio twice in series and confound every measurement.

    Then it installs the APO and prints the status report, which is also how you
    check whether audiodg.exe has loaded the DLL.

    To undo everything:

        target\release\sb-setup.exe uninstall
#>

[CmdletBinding()]
param(
    # Leave the reference product alone (e.g. to re-run just the install).
    [switch] $SkipLetasoft
)

$ErrorActionPreference = 'Stop'

function Assert-Elevated {
    $principal = [Security.Principal.WindowsPrincipal]::new(
        [Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'run this from an elevated prompt: the effect chain and the APO store are machine-wide'
    }
}

$repo = Split-Path -Parent $PSScriptRoot
$setup = Join-Path $repo 'target\release\sb-setup.exe'
$dll = Join-Path $repo 'target\release\sb_apo.dll'
$gain = Join-Path $repo 'target\release\sb-gain.exe'

Assert-Elevated
foreach ($path in @($setup, $dll, $gain)) {
    if (-not (Test-Path $path)) { throw "missing $path -- run 'cargo build --release' first" }
}

if (-not $SkipLetasoft) {
    Write-Host "`n== Letasoft Sound Booster ==" -ForegroundColor Cyan
    $uninstaller = 'C:\Program Files (x86)\Letasoft Sound Booster\unins000.exe'
    if (Test-Path $uninstaller) {
        Write-Host 'uninstalling; its own uninstaller restores the effect chain it recorded'
        $process = Start-Process -FilePath $uninstaller -ArgumentList '/SILENT', '/NORESTART' -Wait -PassThru
        Write-Host "  exit code $($process.ExitCode)"
    }
    else {
        Write-Host 'not installed, nothing to do'
    }
}

Write-Host "`n== installing our APO ==" -ForegroundColor Cyan
& $setup install
if ($LASTEXITCODE -ne 0) { throw "sb-setup install failed with exit code $LASTEXITCODE" }

Write-Host "`n== status ==" -ForegroundColor Cyan
& $setup status

Write-Host "`nNext: play something, then set the level and look at the status again:" -ForegroundColor Green
Write-Host "  `"$gain`" 500"
Write-Host "  `"$setup`" status      # sb_apo.dll should appear in the audiodg.exe list"
