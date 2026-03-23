#Requires -RunAsAdministrator
<#
.SYNOPSIS
    YoLab Windows Installer — sets up NixOS-WSL with the YoLab homelab configuration.

.DESCRIPTION
    1. Enables WSL2 and the Virtual Machine Platform feature
    2. Downloads and imports NixOS-WSL
    3. Registers a Task Scheduler entry so WSL auto-starts at logon
    4. Opens a WSL terminal where the user completes the YoLab setup

.NOTES
    Requires Windows 10 version 2004+ (build 19041+) or Windows 11.
    Run from an elevated (Administrator) PowerShell session.
#>

$ErrorActionPreference = "Stop"

$NixosWslVersion = "24.05"
$NixosWslUrl = "https://github.com/nix-community/NixOS-WSL/releases/latest/download/nixos-wsl.tar.gz"
$NixosInstallDir = "$env:USERPROFILE\NixOS"
$TarPath = "$env:TEMP\nixos-wsl.tar.gz"
$DistroName = "NixOS"
$TaskName = "YoLab-WSL-Autostart"
$YolabGitRepo = "https://github.com/DemyCode/yolab.git"

function Write-Step([string]$msg) {
    Write-Host "`n>>> $msg" -ForegroundColor Cyan
}

function Write-Success([string]$msg) {
    Write-Host "    $msg" -ForegroundColor Green
}

function Write-Warn([string]$msg) {
    Write-Host "    WARNING: $msg" -ForegroundColor Yellow
}

# ─── 1. Check Windows version ────────────────────────────────────────────────
Write-Step "Checking Windows version"
$build = [System.Environment]::OSVersion.Version.Build
if ($build -lt 19041) {
    Write-Host "ERROR: WSL2 requires Windows 10 build 19041 or newer (you have build $build)." -ForegroundColor Red
    exit 1
}
Write-Success "Windows build $build — OK"

# ─── 2. Enable WSL2 features ─────────────────────────────────────────────────
Write-Step "Enabling WSL2 features (may require a reboot)"

$wslFeature = Get-WindowsOptionalFeature -Online -FeatureName "Microsoft-Windows-Subsystem-Linux"
$vmFeature   = Get-WindowsOptionalFeature -Online -FeatureName "VirtualMachinePlatform"

$rebootNeeded = $false

if ($wslFeature.State -ne "Enabled") {
    Enable-WindowsOptionalFeature -Online -FeatureName "Microsoft-Windows-Subsystem-Linux" -NoRestart | Out-Null
    $rebootNeeded = $true
    Write-Success "WSL feature enabled"
} else {
    Write-Success "WSL feature already enabled"
}

if ($vmFeature.State -ne "Enabled") {
    Enable-WindowsOptionalFeature -Online -FeatureName "VirtualMachinePlatform" -NoRestart | Out-Null
    $rebootNeeded = $true
    Write-Success "Virtual Machine Platform enabled"
} else {
    Write-Success "Virtual Machine Platform already enabled"
}

if ($rebootNeeded) {
    Write-Host "`nA reboot is required to finish enabling WSL2." -ForegroundColor Yellow
    Write-Host "After rebooting, run this script again to continue." -ForegroundColor Yellow
    Read-Host "Press Enter to reboot now, or Ctrl+C to reboot manually later"
    Restart-Computer -Force
    exit 0
}

# Set WSL default version to 2
wsl --set-default-version 2 | Out-Null

# ─── 3. Check if NixOS distro already exists ────────────────────────────────
Write-Step "Checking for existing NixOS WSL distro"

$existing = wsl --list --quiet 2>$null | Where-Object { $_ -match $DistroName }
if ($existing) {
    Write-Warn "A '$DistroName' WSL distro already exists. Skipping import."
} else {
    # ─── 4. Download NixOS-WSL ────────────────────────────────────────────────
    Write-Step "Downloading NixOS-WSL (this may take a few minutes)"
    Write-Host "    From: $NixosWslUrl"

    $wc = New-Object System.Net.WebClient
    $wc.DownloadFile($NixosWslUrl, $TarPath)
    Write-Success "Downloaded to $TarPath"

    # ─── 5. Import NixOS-WSL ─────────────────────────────────────────────────
    Write-Step "Importing NixOS WSL distro"
    New-Item -ItemType Directory -Force -Path $NixosInstallDir | Out-Null
    wsl --import $DistroName $NixosInstallDir $TarPath --version 2
    Write-Success "NixOS distro imported to $NixosInstallDir"
    Remove-Item $TarPath -ErrorAction SilentlyContinue
}

# ─── 6. Register Task Scheduler auto-start ───────────────────────────────────
Write-Step "Setting up WSL auto-start at logon"

$existingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($existingTask) {
    Write-Warn "Task '$TaskName' already exists — skipping creation."
} else {
    $action   = New-ScheduledTaskAction -Execute "wsl.exe" -Argument "-d $DistroName"
    $trigger  = New-ScheduledTaskTrigger -AtLogOn
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0 -MultipleInstances IgnoreNew
    $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Highest

    Register-ScheduledTask `
        -TaskName $TaskName `
        -Action $action `
        -Trigger $trigger `
        -Settings $settings `
        -Principal $principal `
        -Description "Starts YoLab NixOS-WSL at user logon" | Out-Null

    Write-Success "Task Scheduler entry created: '$TaskName'"
}

# ─── 7. Run the YoLab setup inside WSL ───────────────────────────────────────
Write-Step "Running YoLab setup inside NixOS-WSL"
Write-Host ""
Write-Host "    The following commands will run inside NixOS-WSL:" -ForegroundColor DarkGray
Write-Host "      1. Update nix channels" -ForegroundColor DarkGray
Write-Host "      2. Clone the YoLab repository to /etc/nixos" -ForegroundColor DarkGray
Write-Host "      3. Launch the interactive YoLab installer" -ForegroundColor DarkGray
Write-Host ""

$setupScript = @"
set -e

echo '>>> Updating nix channels...'
nix-channel --update

echo '>>> Cloning YoLab repository...'
if [ -d /etc/nixos/.git ]; then
    echo '    Repository already exists, skipping clone.'
else
    sudo mkdir -p /etc/nixos
    sudo git clone $YolabGitRepo /etc/nixos
fi

echo '>>> Running YoLab installer (WSL mode)...'
cd /etc/nixos
nix run .#installer -- wsl-setup

echo ''
echo '>>> Setup complete! Applying NixOS configuration...'
sudo nixos-rebuild switch --flake /etc/nixos#yolab-wsl

echo ''
echo 'YoLab is ready. You can access the UI at http://localhost'
"@

# Launch WSL with the setup script
wsl -d $DistroName -- bash -c $setupScript

Write-Host ""
Write-Host "Installation complete!" -ForegroundColor Green
Write-Host "YoLab will start automatically the next time you log in." -ForegroundColor Green
Write-Host "Access the homelab UI at: http://localhost" -ForegroundColor Cyan
