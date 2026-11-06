param(
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist')
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$certificate = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject 'CN=MZip Development' `
    -CertStoreLocation 'Cert:\CurrentUser\My' `
    -KeyExportPolicy NonExportable `
    -HashAlgorithm 'SHA256' `
    -NotAfter (Get-Date).AddYears(2)
$certificatePath = Join-Path $OutputDirectory 'MZip-Development.cer'
Export-Certificate -Cert $certificate -FilePath $certificatePath | Out-Null

Write-Output "Certificate thumbprint: $($certificate.Thumbprint)"
Write-Output "Public certificate: $certificatePath"
Write-Output 'The certificate has NOT been added to any trusted store.'
Write-Output "Build with: .\installer\build.ps1 -CertificateThumbprint $($certificate.Thumbprint)"
