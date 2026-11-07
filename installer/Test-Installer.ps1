param(
    [Parameter(Mandatory = $true)]
    [string]$Package,
    [string]$PreviousPackage,
    [switch]$RunLifecycle
)

$ErrorActionPreference = 'Stop'
$packagePath = (Resolve-Path -LiteralPath $Package).Path
$projectRoot = Split-Path -Parent $PSScriptRoot
$installer = New-Object -ComObject WindowsInstaller.Installer

function Read-MsiRows($Database, [string]$Query, [int]$Columns) {
    $view = $Database.OpenView($Query)
    try {
        [void]$view.Execute()
        while ($null -ne ($record = $view.Fetch())) {
            $values = for ($column = 1; $column -le $Columns; $column++) {
                $record.StringData($column)
            }
            [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($record)
            [PSCustomObject]@{ Values = @($values) }
        }
    } finally {
        [void]$view.Close()
        [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($view)
    }
}

function Get-MsiProperty($Database, [string]$Name) {
    $rows = @(Read-MsiRows $Database "SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$Name'" 1)
    if ($rows.Count -ne 1) {
        throw "Expected one MSI property: $Name"
    }
    return $rows[0].Values[0]
}

function Assert-Installer([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
    Write-Output "PASS: $Message"
}

function Get-InstalledHx6Zip {
    foreach ($code in $installer.RelatedProducts($upgradeCode)) {
        if ($installer.ProductState($code) -eq 5) {
            [PSCustomObject]@{
                PSChildName = $code
                DisplayVersion = $installer.ProductInfo($code, 'VersionString')
            }
        }
    }
}

function Invoke-TestMsi([string[]]$Arguments, [string]$Log, [switch]$ExpectFailure) {
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList (
        $Arguments + @('/qn', '/norestart', '/l*v', "`"$Log`"")
    ) -Wait -PassThru
    $succeeded = $process.ExitCode -in @(0, 3010)
    if ($ExpectFailure) {
        Assert-Installer (-not $succeeded) "Installation is rejected ($([System.IO.Path]::GetFileName($Log)))"
    } elseif (-not $succeeded) {
        throw "Windows Installer failed with exit code $($process.ExitCode). See $Log"
    }
}

function Invoke-MsiSql($Database, [string]$Query) {
    $view = $Database.OpenView($Query)
    try {
        [void]$view.Execute()
    } finally {
        [void]$view.Close()
        [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($view)
    }
}

$database = $installer.OpenDatabase($packagePath, 0)
$version = Get-MsiProperty $database 'ProductVersion'
$productCode = Get-MsiProperty $database 'ProductCode'
$upgradeCode = Get-MsiProperty $database 'UpgradeCode'
Assert-Installer ((Get-MsiProperty $database 'ProductLanguage') -eq '2052') 'Installer UI is Simplified Chinese'

$dialogs = @(Read-MsiRows $database 'SELECT `Dialog` FROM `Dialog`' 1)
foreach ($dialog in @('WelcomeDlg', 'InstallDirDlg', 'MaintenanceWelcomeDlg', 'MaintenanceTypeDlg',
    'VerifyReadyDlg', 'ProgressDlg', 'ExitDialog')) {
    Assert-Installer ($dialog -in @($dialogs | ForEach-Object { $_.Values[0] })) "Wizard includes $dialog"
}
$events = @(Read-MsiRows $database 'SELECT `Dialog_`,`Control_`,`Event`,`Argument`,`Condition`,`Ordering` FROM `ControlEvent`' 6)
foreach ($route in @(
    @{ Dialog = 'WelcomeDlg'; Control = 'Next'; Target = 'InstallDirDlg' },
    @{ Dialog = 'InstallDirDlg'; Control = 'Back'; Target = 'WelcomeDlg' },
    @{ Dialog = 'MaintenanceTypeDlg'; Control = 'RepairButton'; Target = 'VerifyReadyDlg' },
    @{ Dialog = 'MaintenanceTypeDlg'; Control = 'RemoveButton'; Target = 'VerifyReadyDlg' }
)) {
    $matches = @($events | Where-Object {
        $_.Values[0] -eq $route.Dialog -and $_.Values[1] -eq $route.Control -and
        $_.Values[2] -eq 'NewDialog' -and $_.Values[4] -ne 'WIX_UPGRADE_DETECTED'
    } | Sort-Object { [int]$_.Values[5] } -Descending)
    Assert-Installer ($matches.Count -gt 0 -and $matches[0].Values[3] -eq $route.Target) (
        "$($route.Dialog)/$($route.Control) navigates to $($route.Target)"
    )
}
$upgradeRoute = @($events | Where-Object {
    $_.Values[0] -eq 'WelcomeDlg' -and $_.Values[1] -eq 'Next' -and
    $_.Values[2] -eq 'NewDialog' -and $_.Values[3] -eq 'VerifyReadyDlg' -and
    $_.Values[4] -eq 'WIX_UPGRADE_DETECTED' -and [int]$_.Values[5] -gt 10
})
Assert-Installer ($upgradeRoute.Count -eq 1) 'Upgrade requires confirmation without offering a directory change'
$upgrades = @(Read-MsiRows $database 'SELECT `UpgradeCode`,`VersionMax`,`Language`,`Attributes`,`ActionProperty` FROM `Upgrade`' 5)
$replacement = @($upgrades | Where-Object { $_.Values[4] -eq 'WIX_UPGRADE_DETECTED' })
Assert-Installer ($replacement.Count -eq 1 -and $replacement[0].Values[0] -eq $upgradeCode -and
    $replacement[0].Values[1] -eq $version -and $replacement[0].Values[2] -eq '' -and
    ([int]$replacement[0].Values[3] -band 512) -ne 0) 'Upgrade detects older and rebuilt same-version packages regardless of language'
$sequence = @{}
Read-MsiRows $database 'SELECT `Action`,`Sequence` FROM `InstallExecuteSequence`' 2 |
    ForEach-Object { $sequence[$_.Values[0]] = [int]$_.Values[1] }
Assert-Installer ($sequence['RemoveExistingProducts'] -gt $sequence['InstallExecute'] -and
    $sequence['RemoveExistingProducts'] -lt $sequence['InstallFinalize']) 'Old product is retained until the new installation script succeeds'
Assert-Installer ((Get-MsiProperty $database 'REINSTALLMODE') -eq 'amus') 'Same-version replacement overwrites equal-version executable files'
$currentComponents = @(Read-MsiRows $database 'SELECT `Component`,`ComponentId`,`Directory_`,`Attributes`,`KeyPath` FROM `Component`' 5)
$registry = @(Read-MsiRows $database 'SELECT `Root`,`Key`,`Name`,`Value` FROM `Registry`' 4)
$eachZipKey = 'Software\Classes\AllFilesystemObjects\shell\Hx6Zip\shell\10.CreateEachZip'
$eachZipClass = '{F4774D42-41E4-4AC8-B964-9A37688AE104}'
$eachZipTitle = [string]::new([char[]]@(0x538B, 0x7F29, 0x6BCF, 0x4E2A, 0x5230, 0x5355,
    0x72EC, 0x7684, 0x538B, 0x7F29, 0x5305, 0x4E2D))
$title = @($registry | Where-Object {
    $_.Values[0] -eq '1' -and $_.Values[1] -eq $eachZipKey -and
    $_.Values[2] -eq 'MUIVerb' -and $_.Values[3] -eq $eachZipTitle
})
Assert-Installer ($title.Count -eq 1) 'Separate ZIP traditional menu has the expected Unicode title'
$model = @($registry | Where-Object {
    $_.Values[0] -eq '1' -and $_.Values[1] -eq $eachZipKey -and
    $_.Values[2] -eq 'MultiSelectModel' -and $_.Values[3] -eq 'Player'
})
Assert-Installer ($model.Count -eq 1) 'Separate ZIP traditional menu supports multi-selection'
$delegate = @($registry | Where-Object {
    $_.Values[0] -eq '1' -and $_.Values[1] -eq "$eachZipKey\command" -and
    $_.Values[2] -eq 'DelegateExecute' -and $_.Values[3] -eq $eachZipClass
})
Assert-Installer ($delegate.Count -eq 1) 'Separate ZIP traditional menu uses DelegateExecute'
$server = @($registry | Where-Object {
    $_.Values[0] -eq '1' -and $_.Values[1] -eq "Software\Classes\CLSID\$eachZipClass\LocalServer32" -and
    $_.Values[2] -eq '' -and $_.Values[3] -eq '"[INSTALLFOLDER]hx6zip.exe" --shell-server'
})
Assert-Installer ($server.Count -eq 1) 'Separate ZIP delegate starts the executable Shell server'
$legacy = @($registry | Where-Object {
    $_.Values[1] -eq $eachZipKey -and $_.Values[2] -eq 'ExplorerCommandHandler'
})
Assert-Installer ($legacy.Count -eq 0) 'Separate ZIP traditional menu does not depend on the DLL handler'
$removals = @(Read-MsiRows $database 'SELECT `Root`,`Key`,`Name` FROM `RemoveRegistry`' 3)
$cleanup = @($removals | Where-Object {
    $_.Values[0] -eq '1' -and $_.Values[1] -eq $eachZipKey -and
    $_.Values[2] -eq 'ExplorerCommandHandler'
})
Assert-Installer ($cleanup.Count -eq 1) 'Upgrade removes the previous separate ZIP DLL handler value'
Assert-Installer ($sequence['RemoveRegistryValues'] -gt 0 -and
    $sequence['RemoveRegistryValues'] -lt $sequence['WriteRegistryValues']) 'Legacy registry cleanup runs before new values are written'
[void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($database)

if (-not $RunLifecycle) {
    Write-Output 'Static MSI checks complete. Use -RunLifecycle -PreviousPackage <older.msi> for installation tests.'
    return
}
if (-not $PreviousPackage) {
    throw '-RunLifecycle requires -PreviousPackage pointing to an older Hx6Zip MSI.'
}
$previousPath = (Resolve-Path -LiteralPath $PreviousPackage).Path
$previousDatabase = $installer.OpenDatabase($previousPath, 0)
$previousCode = Get-MsiProperty $previousDatabase 'ProductCode'
Assert-Installer ([Version](Get-MsiProperty $previousDatabase 'ProductVersion') -lt [Version]$version) 'Baseline package is older'
Assert-Installer ((Get-MsiProperty $previousDatabase 'UpgradeCode') -eq $upgradeCode) 'Baseline belongs to the same upgrade family'
$previousComponents = @(Read-MsiRows $previousDatabase 'SELECT `Component`,`ComponentId`,`Directory_`,`Attributes`,`KeyPath` FROM `Component`' 5)
foreach ($component in $previousComponents) {
    $current = @($currentComponents | Where-Object { $_.Values[0] -eq $component.Values[0] })
    Assert-Installer ($current.Count -eq 1 -and ($current[0].Values -join '|') -eq ($component.Values -join '|')) (
        "Late upgrade preserves component identity and key path: $($component.Values[0])"
    )
}
[void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($previousDatabase)
if (@(Get-InstalledHx6Zip).Count -ne 0 -or (Test-Path -LiteralPath 'HKCU:\Software\Hx6Zip') -or
    @(Get-AppxPackage -Name 'MZip.Desktop.Dev' -ErrorAction Stop).Count -ne 0) {
    throw 'Lifecycle tests require no existing Hx6Zip installation, registry settings, or registered menu package. Use a disposable Windows account or VM.'
}
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("Hx6Zip-installer-test-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
$work = (Resolve-Path -LiteralPath $work).Path
$installFolder = Join-Path $work 'application'
$installedExecutable = Join-Path $installFolder 'hx6zip.exe'
$testCodes = @($previousCode, $productCode)
$passed = $false
try {
    $repackedDirectory = Join-Path $work 'repacked'
    & (Join-Path $PSScriptRoot 'build.ps1') -SkipBuild -OutputDirectory $repackedDirectory
    $repackedPath = Join-Path $repackedDirectory "Hx6Zip-Setup-$version-x64.msi"
    $repackedDatabase = $installer.OpenDatabase($repackedPath, 0)
    $repackedCode = Get-MsiProperty $repackedDatabase 'ProductCode'
    [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($repackedDatabase)
    $testCodes += $repackedCode
    Assert-Installer ($repackedCode -ne $productCode) 'Repackaged MSI has a new product identity'

    Invoke-TestMsi @('/i', "`"$previousPath`"", "INSTALLFOLDER=`"$installFolder`"") (Join-Path $work '01-baseline.log')
    Assert-Installer ((Test-Path -LiteralPath $installedExecutable) -and @(Get-InstalledHx6Zip).Count -eq 1) 'Baseline installs once'

    Invoke-TestMsi @('/i', "`"$packagePath`"") (Join-Path $work '02-upgrade.log')
    $installed = @(Get-InstalledHx6Zip)
    Assert-Installer ($installed.Count -eq 1 -and $installed[0].PSChildName -eq $productCode) 'Upgrade removes the legacy product and leaves one uninstall entry'
    $expectedHash = (Get-FileHash -LiteralPath (Join-Path $projectRoot 'target\release\hx6zip.exe')).Hash
    Assert-Installer ((Get-FileHash -LiteralPath $installedExecutable).Hash -eq $expectedHash) 'Upgrade replaces the executable with the new release'

    Invoke-TestMsi @('/i', "`"$packagePath`"") (Join-Path $work '03-repeat.log')
    Assert-Installer (@(Get-InstalledHx6Zip).Count -eq 1) 'Reopening the installed package does not create duplicates'
    Remove-Item -LiteralPath $installedExecutable
    Invoke-TestMsi @('/fa', "`"$packagePath`"") (Join-Path $work '04-repair.log')
    Assert-Installer ((Get-FileHash -LiteralPath $installedExecutable).Hash -eq $expectedHash) 'Repair restores the missing executable'

    $modified = [System.IO.File]::Open($installedExecutable, [System.IO.FileMode]::Append)
    try {
        $modified.WriteByte(0)
    } finally {
        $modified.Dispose()
    }
    Assert-Installer ([System.Diagnostics.FileVersionInfo]::GetVersionInfo($installedExecutable).ProductVersion -eq $version -and
        (Get-FileHash -LiteralPath $installedExecutable).Hash -ne $expectedHash) 'Replacement test uses a changed executable with the same embedded version'
    $installedMenuKey = "HKCU:\$eachZipKey"
    [void](New-ItemProperty -LiteralPath $installedMenuKey -Name 'ExplorerCommandHandler' `
        -Value '{F4774D42-41E4-4AC8-B964-9A37688AE103}' -PropertyType String -Force)
    Remove-ItemProperty -LiteralPath "$installedMenuKey\command" -Name 'DelegateExecute'
    Assert-Installer ((Get-ItemProperty -LiteralPath $installedMenuKey).ExplorerCommandHandler -eq
        '{F4774D42-41E4-4AC8-B964-9A37688AE103}') 'Same-version replacement test starts with the legacy DLL menu handler'
    Invoke-TestMsi @('/i', "`"$repackedPath`"") (Join-Path $work '05-same-version.log')
    $installed = @(Get-InstalledHx6Zip)
    Assert-Installer ($installed.Count -eq 1 -and $installed[0].PSChildName -eq $repackedCode) 'Same-version rebuild replaces the previous package instead of installing alongside it'
    Assert-Installer ((Get-FileHash -LiteralPath $installedExecutable).Hash -eq $expectedHash) 'Same-version upgrade preserves the selected directory and installs the correct executable'
    $installedMenu = Get-ItemProperty -LiteralPath $installedMenuKey
    $installedCommand = Get-ItemProperty -LiteralPath "$installedMenuKey\command"
    Assert-Installer ($null -eq $installedMenu.PSObject.Properties['ExplorerCommandHandler'] -and
        $installedMenu.MUIVerb -eq $eachZipTitle -and $installedMenu.MultiSelectModel -eq 'Player' -and
        $installedCommand.DelegateExecute -eq $eachZipClass) 'Same-version upgrade replaces the legacy handler with the working delegate'

    Invoke-TestMsi @('/i', "`"$previousPath`"") (Join-Path $work '06-downgrade.log') -ExpectFailure
    $installed = @(Get-InstalledHx6Zip)
    Assert-Installer ($installed.Count -eq 1 -and $installed[0].PSChildName -eq $repackedCode -and
        (Get-FileHash -LiteralPath $installedExecutable).Hash -eq $expectedHash) 'Rejected downgrade leaves the installed release intact'

    $failurePath = Join-Path $work 'rollback-test.msi'
    Copy-Item -LiteralPath $packagePath -Destination $failurePath
    $modified = [System.IO.File]::Open($installedExecutable, [System.IO.FileMode]::Append)
    try {
        $modified.WriteByte(1)
    } finally {
        $modified.Dispose()
    }
    $rollbackHash = (Get-FileHash -LiteralPath $installedExecutable).Hash
    $failureDatabase = $installer.OpenDatabase($failurePath, 1)
    try {
        $failureCode = '{' + [Guid]::NewGuid().ToString().ToUpperInvariant() + '}'
        $testCodes += $failureCode
        Invoke-MsiSql $failureDatabase "UPDATE ``Property`` SET ``Value``='$failureCode' WHERE ``Property``='ProductCode'"
        # A deferred EXE failure runs after file copies, before the old product is removed.
        Invoke-MsiSql $failureDatabase "INSERT INTO ``CustomAction`` (``Action``,``Type``,``Source``,``Target``) VALUES ('Hx6ZipTestFailure',1058,'SystemFolder','[SystemFolder]cmd.exe /c exit 1')"
        Invoke-MsiSql $failureDatabase "INSERT INTO ``InstallExecuteSequence`` (``Action``,``Condition``,``Sequence``) VALUES ('Hx6ZipTestFailure','1',4001)"
        [void]$failureDatabase.Commit()
    } finally {
        [void][System.Runtime.InteropServices.Marshal]::FinalReleaseComObject($failureDatabase)
    }
    $rollbackLog = Join-Path $work '07-rollback.log'
    Invoke-TestMsi @('/i', "`"$failurePath`"") $rollbackLog -ExpectFailure
    Assert-Installer ([bool](Select-String -LiteralPath $rollbackLog -SimpleMatch 'CustomAction Hx6ZipTestFailure returned actual error code 1' -Quiet)) 'Rollback test reaches the deferred failure after replacing files'
    $installed = @(Get-InstalledHx6Zip)
    Assert-Installer ($installed.Count -eq 1 -and $installed[0].PSChildName -eq $repackedCode -and
        (Get-FileHash -LiteralPath $installedExecutable).Hash -eq $rollbackHash) 'Failed upgrade restores the previous product registration and exact previous executable bytes'

    Invoke-TestMsi @('/x', $repackedCode) (Join-Path $work '08-uninstall.log')
    Assert-Installer (@(Get-InstalledHx6Zip).Count -eq 0 -and -not (Test-Path -LiteralPath $installedExecutable) -and
        -not (Test-Path -LiteralPath 'HKCU:\Software\Classes\AllFilesystemObjects\shell\Hx6Zip') -and
        -not (Test-Path -LiteralPath 'HKCU:\Software\Classes\Hx6Zip.Archive.1')) 'Uninstall removes the application, uninstall entry, menu, and file association'
    $passed = $true
} finally {
    foreach ($installed in @(Get-InstalledHx6Zip)) {
        if ($installed.PSChildName -in $testCodes) {
            Invoke-TestMsi @('/x', $installed.PSChildName) (Join-Path $work "cleanup-$($installed.PSChildName).log")
        }
    }
    if ($passed) {
        Remove-Item -LiteralPath $work -Recurse -Force
    } else {
        Write-Warning "Installer test logs retained at $work"
    }
}
