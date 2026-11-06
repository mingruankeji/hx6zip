param(
    [switch]$Unregister,
    [switch]$TrustCertificate,
    [int]$UILevel = 0
)

$ErrorActionPreference = 'Stop'
$packageName = 'MZip.Desktop.Dev'
$publisher = 'CN=MZip Development'
$logPath = Join-Path ([System.IO.Path]::GetTempPath()) 'MZip-Windows11Menu.log'

function Write-MenuLog([string]$Message) {
    Write-Output $Message
    if ($TrustCertificate) {
        Add-Content -LiteralPath $logPath -Value "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $Message" -Encoding UTF8
    }
}

function Get-MZipPackages {
    @(Get-AppxPackage -Name $packageName -ErrorAction Stop | Where-Object { $_.Publisher -eq $publisher })
}

function Get-PackageVersion([string]$Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $reader = New-Object System.IO.StreamReader ($archive.GetEntry('AppxManifest.xml').Open())
        try {
            return ([xml]$reader.ReadToEnd()).Package.Identity.Version
        } finally {
            $reader.Dispose()
        }
    } finally {
        $archive.Dispose()
    }
}

function Test-Administrator {
    $principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Confirm-CertificateTrust([string]$CertificatePath) {
    $certificate = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $CertificatePath
    if ($certificate.Subject -ne $publisher) {
        throw "Unexpected certificate subject: $($certificate.Subject)"
    }
    $storePath = "Cert:\LocalMachine\TrustedPeople\$($certificate.Thumbprint)"
    if (Test-Path -LiteralPath $storePath) {
        Write-MenuLog "Certificate $($certificate.Thumbprint) is already trusted."
        return
    }

    # Record ownership so uninstall only removes trust that MZip itself added.
    $import = "Import-Certificate -FilePath '$($CertificatePath.Replace("'", "''"))' -CertStoreLocation 'Cert:\LocalMachine\TrustedPeople' | Out-Null; " +
        "New-Item -Path 'HKLM:\SOFTWARE\MZip\CertificateTrust' -Force | Out-Null; " +
        "Set-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\MZip\CertificateTrust' -Name Thumbprint -Value '$($certificate.Thumbprint)'"
    if (Test-Administrator) {
        Invoke-Expression $import
    } else {
        Write-MenuLog "Requesting administrator approval to trust certificate $($certificate.Thumbprint)."
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("`$ErrorActionPreference = 'Stop'; $import"))
        try {
            $process = Start-Process -FilePath "$PSHOME\powershell.exe" -Verb RunAs -Wait -PassThru -WindowStyle Hidden `
                -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded)
        } catch {
            throw "Administrator approval was not granted, so the certificate was not trusted. $($_.Exception.Message)"
        }
        if ($process.ExitCode -ne 0) {
            throw "Certificate import exited with code $($process.ExitCode)."
        }
    }
    if (-not (Test-Path -LiteralPath $storePath)) {
        throw 'The certificate was not added to Local Machine > Trusted People.'
    }
    Write-MenuLog "Certificate $($certificate.Thumbprint) added to Local Machine > Trusted People."
}

function Register-MenuPackage {
    if ([Environment]::OSVersion.Version.Build -lt 22000) {
        Write-MenuLog 'Windows 10 detected; the traditional context menu is used.'
        return
    }
    $packagePath = Join-Path $PSScriptRoot 'MZip-Menu.msix'
    if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
        throw "Signed MZip menu package not found: $packagePath. Run the copy of this script in the MZip installation folder (default: $env:LOCALAPPDATA\Programs\MZip), not the source template; if it is missing there, rebuild the installer with a code-signing certificate."
    }
    if ($TrustCertificate) {
        Confirm-CertificateTrust (Join-Path $PSScriptRoot 'MZip-Development.cer')
    }

    # A rebuilt package may reuse its version, which Windows rejects as a conflicting update.
    $version = Get-PackageVersion $packagePath
    foreach ($package in Get-MZipPackages) {
        if ($package.Version -eq $version) {
            Remove-AppxPackage -Package $package.PackageFullName -ErrorAction Stop
        }
    }
    try {
        Add-AppxPackage -Path $packagePath -ExternalLocation $PSScriptRoot -ErrorAction Stop
    } catch {
        throw "Unable to register the signed MZip menu package. Check that you trust its signing certificate, then retry. Windows reported: $($_.Exception.Message)"
    }
    Write-MenuLog 'MZip Windows 11 context menu registered for the current user.'
    Write-MenuLog 'Restart File Explorer, or sign out and back in, if the menu does not appear.'
}

# The repository copy is only a template; the menu package exists solely in the installation folder.
if (-not (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'MZip-Menu.msix') -PathType Leaf) -and -not $Unregister) {
    $installDir = (Get-ItemProperty -LiteralPath 'HKCU:\Software\MZip' -ErrorAction SilentlyContinue).InstallDir
    if (-not $installDir) { $installDir = Join-Path $env:LOCALAPPDATA 'Programs\MZip' }
    $installedScript = Join-Path $installDir 'Windows11Menu.ps1'
    $installedPackage = Join-Path $installDir 'MZip-Menu.msix'
    if ((Test-Path -LiteralPath $installedPackage -PathType Leaf) -and
        [IO.Path]::GetFullPath($installDir).TrimEnd('\') -ne [IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\')) {
        Write-Output "Running the installed copy: $installedScript"
        & $installedScript -TrustCertificate:$TrustCertificate -UILevel $UILevel
        return
    }
    throw "MZip is not installed with the Windows 11 menu (no $installedPackage). Install dist\MZip-Setup-*.msi built with -CertificateThumbprint; it trusts the certificate and registers the menu automatically."
}

if ($Unregister) {
    foreach ($package in Get-MZipPackages) {
        Remove-AppxPackage -Package $package.PackageFullName -ErrorAction Stop
        Write-Output "Removed $($package.PackageFullName)"
    }
    return
}

if (-not $TrustCertificate) {
    Register-MenuPackage
    return
}

try {
    Register-MenuPackage
} catch {
    Write-MenuLog "ERROR: $($_.Exception.Message)"
    # Windows Installer full or reduced UI; silent installs only write the log.
    if ($UILevel -ge 4) {
        Add-Type -AssemblyName System.Windows.Forms
        [void][System.Windows.Forms.MessageBox]::Show(
            "MZip 已安装，但未能启用 Windows 11 新版右键菜单，传统菜单（「显示更多选项」）仍可使用。`n`n原因：$($_.Exception.Message)`n`n可稍后重新运行安装程序选择「修复」重试。日志：$logPath",
            'MZip 安装', 'OK', 'Warning')
    }
    exit 1
}