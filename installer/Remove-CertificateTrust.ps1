# Build-time template: build.ps1 replaces __THUMBPRINT__ and embeds this script in the MSI,
# because the uninstall action runs after installed files have been removed.
$ErrorActionPreference = 'Stop'
$thumbprint = '__THUMBPRINT__'
$ownerKey = 'HKLM:\SOFTWARE\MZip\CertificateTrust'

# Only remove a certificate that an MZip installation imported, never one trusted beforehand.
if ((Get-ItemProperty -LiteralPath $ownerKey -ErrorAction SilentlyContinue).Thumbprint -ne $thumbprint) { exit }
if (-not (Test-Path -LiteralPath "Cert:\LocalMachine\TrustedPeople\$thumbprint")) { exit }

$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$elevated = @"
`$ErrorActionPreference = 'Stop'
`$users = @(Get-AppxPackage -AllUsers -Name 'MZip.Desktop.Dev' | Where-Object { `$_.Publisher -eq 'CN=MZip Development' } |
    ForEach-Object { `$_.PackageUserInformation } | Where-Object { `$_.InstallState -eq 'Installed' -and `$_.UserSecurityId.Sid -ne '$sid' })
if (`$users.Count -gt 0) { exit }
Remove-Item -LiteralPath 'Cert:\LocalMachine\TrustedPeople\$thumbprint'
Remove-Item -LiteralPath '$ownerKey' -Recurse
if (-not (Get-ChildItem -LiteralPath 'HKLM:\SOFTWARE\MZip') -and -not (Get-Item -LiteralPath 'HKLM:\SOFTWARE\MZip').Property) {
    Remove-Item -LiteralPath 'HKLM:\SOFTWARE\MZip'
}
"@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($elevated))
Start-Process -FilePath "$PSHOME\powershell.exe" -Verb RunAs -Wait -WindowStyle Hidden `
    -ArgumentList @('-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded)
