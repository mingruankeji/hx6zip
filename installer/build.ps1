param(
    [switch]$SkipBuild,
    [string]$CertificateThumbprint,
    [switch]$Unsigned,
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist')
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"') {
    throw 'Unable to read the package version from Cargo.toml.'
}
$version = $Matches[1]
if ($version -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
    throw 'MSI releases require a three-part numeric Cargo version (major.minor.patch), without prerelease or build metadata.'
}
$msiVersion = [Version]$version
if ($msiVersion.Major -gt 255 -or $msiVersion.Minor -gt 255 -or $msiVersion.Build -gt 65535) {
    throw 'The Cargo version exceeds MSI version limits (255.255.65535).'
}
$executable = Join-Path $projectRoot 'target\release\hx6zip.exe'
$shellDll = Join-Path $projectRoot 'target\release\hx6zip_shell_extension.dll'

if (-not $SkipBuild) {
    Push-Location $projectRoot
    try {
        cargo build --release --bin hx6zip --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Release build failed with exit code $LASTEXITCODE."
        }
        cargo build --release -p hx6zip-shell-extension --locked
        if ($LASTEXITCODE -ne 0) {
            throw "Shell extension build failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
    }
}

foreach ($required in @($executable, $shellDll)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Release build output not found: $required"
    }
}
$executableVersion = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($executable).ProductVersion
if ($executableVersion -ne $version) {
    throw "Executable version '$executableVersion' does not match Cargo version '$version'. Rebuild without -SkipBuild."
}

$wix = Get-Command wix.exe -ErrorAction SilentlyContinue
if (-not $wix) {
    throw 'WiX Toolset 6 is required. Install it with: dotnet tool install --global wix'
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$msi = Join-Path $OutputDirectory "Hx6Zip-Setup-$version-x64.msi"
$portable = Join-Path $OutputDirectory "Hx6Zip-Portable-$version-x64.zip"
$wxs = Join-Path $PSScriptRoot 'Package.wxs'
$notices = Join-Path $projectRoot 'THIRD_PARTY_NOTICES.txt'
$menuScript = Join-Path $PSScriptRoot 'Windows11Menu.ps1'
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("Hx6Zip-build-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null

try {
    $wixArguments = @(
        'build', $wxs,
        '-arch', 'x64',
        '-ext', 'WixToolset.UI.wixext',
        '-culture', 'zh-CN',
        '-d', "Hx6ZipExe=$executable",
        '-d', "Hx6ZipShellDll=$shellDll",
        '-d', "Hx6ZipMenuScript=$menuScript",
        '-d', "Hx6ZipNotices=$notices",
        '-d', "Hx6ZipIcon=$(Join-Path $projectRoot 'assets\hx6zip.ico')",
        '-d', "Hx6ZipVersion=$version"
    )

    if (-not $CertificateThumbprint -and -not $Unsigned) {
        # Reuse the development certificate so a plain build still produces the Windows 11 menu.
        $detected = Get-ChildItem -LiteralPath 'Cert:\CurrentUser\My' |
            Where-Object {
                $_.Subject -eq 'CN=MZip Development' -and $_.HasPrivateKey -and
                $_.NotBefore -le (Get-Date) -and $_.NotAfter -gt (Get-Date) -and
                ($_.EnhancedKeyUsageList | Where-Object { $_.ObjectId -eq '1.3.6.1.5.5.7.3.3' })
            } |
            Sort-Object NotAfter -Descending |
            Select-Object -First 1
        if ($detected) {
            $CertificateThumbprint = $detected.Thumbprint
            Write-Output "Using code-signing certificate $CertificateThumbprint (CN=MZip Development)."
        }
    }

    if ($CertificateThumbprint) {
        $thumbprint = $CertificateThumbprint.Replace(' ', '').ToUpperInvariant()
        if ($thumbprint -notmatch '^[0-9A-F]{40}$') {
            throw 'CertificateThumbprint must be a SHA-1 certificate thumbprint.'
        }
        $certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$thumbprint"
        if ($certificate.Subject -ne 'CN=MZip Development' -or -not $certificate.HasPrivateKey) {
            throw 'The certificate must have subject CN=MZip Development and an accessible private key.'
        }
        if ($certificate.NotBefore -gt (Get-Date) -or $certificate.NotAfter -le (Get-Date)) {
            throw 'The code-signing certificate is not currently valid.'
        }
        if (-not ($certificate.EnhancedKeyUsageList | Where-Object { $_.ObjectId -eq '1.3.6.1.5.5.7.3.3' })) {
            throw 'The certificate does not permit code signing.'
        }

        $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
        $sdk = Get-ChildItem -LiteralPath $sdkRoot -Directory |
            Where-Object {
                (Test-Path -LiteralPath (Join-Path $_.FullName 'x64\makeappx.exe')) -and
                (Test-Path -LiteralPath (Join-Path $_.FullName 'x64\makepri.exe')) -and
                (Test-Path -LiteralPath (Join-Path $_.FullName 'x64\signtool.exe'))
            } |
            Sort-Object { [Version]$_.Name } -Descending |
            Select-Object -First 1
        if (-not $sdk) {
            throw 'Windows SDK makeappx.exe, makepri.exe and signtool.exe are required for the Windows 11 menu.'
        }

        $stage = Join-Path $work 'package'
        $assets = Join-Path $stage 'Assets'
        New-Item -ItemType Directory -Path $assets -Force | Out-Null
        $appxVersion = "$version.0"
        $appxManifest = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'AppxManifest.xml') -Raw).Replace('__VERSION__', $appxVersion)
        Set-Content -LiteralPath (Join-Path $stage 'AppxManifest.xml') -Value $appxManifest -Encoding UTF8

        foreach ($logo in @('StoreLogo.png', 'Square44x44Logo.png', 'Square150x150Logo.png')) {
            Copy-Item -LiteralPath (Join-Path $projectRoot "assets\$logo") -Destination $assets
        }

        # Processes started from the Windows 11 menu inherit the package identity, so the
        # taskbar uses these target-size logos instead of the executable's window icon.
        Add-Type -AssemblyName System.Drawing
        $logoSource = [System.Drawing.Image]::FromFile((Join-Path $projectRoot 'assets\hx6zip.png'))
        try {
            foreach ($size in @(16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256)) {
                $bitmap = New-Object System.Drawing.Bitmap($size, $size)
                $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
                try {
                    $graphics.Clear([System.Drawing.Color]::Transparent)
                    $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                    $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                    $graphics.DrawImage($logoSource, 0, 0, $size, $size)
                    foreach ($suffix in @('', '_altform-unplated', '_altform-lightunplated')) {
                        $bitmap.Save((Join-Path $assets "Square44x44Logo.targetsize-$size$suffix.png"), [System.Drawing.Imaging.ImageFormat]::Png)
                    }
                } finally {
                    $graphics.Dispose()
                    $bitmap.Dispose()
                }
            }
        } finally {
            $logoSource.Dispose()
        }

        $makePri = Join-Path $sdk.FullName 'x64\makepri.exe'
        $priConfig = Join-Path $work 'priconfig.xml'
        & $makePri createconfig /cf $priConfig /dq zh-CN /pv 10.0.0 /o
        if ($LASTEXITCODE -ne 0) {
            throw "PRI configuration failed with exit code $LASTEXITCODE."
        }
        & $makePri new /pr $stage /cf $priConfig /mn (Join-Path $stage 'AppxManifest.xml') /of (Join-Path $stage 'resources.pri') /o
        if ($LASTEXITCODE -ne 0) {
            throw "PRI resource index failed with exit code $LASTEXITCODE."
        }

        $package = Join-Path $work 'Hx6Zip-Menu.msix'
        $makeAppx = Join-Path $sdk.FullName 'x64\makeappx.exe'
        # The executable and COM DLL live in the MSI's external location.
        & $makeAppx pack /d $stage /p $package /o /nv
        if ($LASTEXITCODE -ne 0) {
            throw "Sparse package build failed with exit code $LASTEXITCODE."
        }
        $signTool = Join-Path $sdk.FullName 'x64\signtool.exe'
        & $signTool sign /fd SHA256 /s My /sha1 $thumbprint $package
        if ($LASTEXITCODE -ne 0) {
            throw "Sparse package signing failed with exit code $LASTEXITCODE."
        }
        $publicCertificate = Join-Path $OutputDirectory 'Hx6Zip-Development.cer'
        Export-Certificate -Cert $certificate -FilePath $publicCertificate | Out-Null
        $removeTrust = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'Remove-CertificateTrust.ps1') -Raw).Replace('__THUMBPRINT__', $thumbprint)
        $removeTrustCommand = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($removeTrust))
        $wixArguments += @(
            '-d', "Hx6ZipRemoveTrustCommand=$removeTrustCommand",
            '-d', 'Hx6ZipHasSignedPackage=1',
            '-d', "Hx6ZipSparsePackage=$package",
            '-d', "Hx6ZipCertificate=$publicCertificate"
        )
    } else {
        $wixArguments += @('-d', 'Hx6ZipHasSignedPackage=0')
        Write-Warning 'No signing certificate was provided. The installer will use the traditional menu only.'
    }

    $wixArguments += @('-pdbtype', 'none', '-o', $msi)
    & $wix.Source @wixArguments
    if ($LASTEXITCODE -ne 0) {
        throw "WiX build failed with exit code $LASTEXITCODE."
    }

    Compress-Archive -LiteralPath @(
        $executable,
        (Join-Path $projectRoot 'README.md'),
        $notices
    ) -DestinationPath $portable -Force
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force
}

Write-Output "Installer: $msi"
Write-Output "Portable:  $portable"
if ($CertificateThumbprint) {
    Write-Output "Public certificate: $publicCertificate"
    Write-Output 'The MSI asks for administrator approval to trust this certificate and registers the Windows 11 menu during installation.'
}
