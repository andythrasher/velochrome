# Builds the Microsoft Store package for Velochrome into artifacts\store.
#
# Usage:
#   powershell -File tools/package.ps1                 # artifacts\store\Velochrome_<version>_x64.msix and .msixupload
#   powershell -File tools/package.ps1 -Sideload       # ...plus Velochrome_<version>_x64_test.msix, signed with a
#                                                      #    self-signed test certificate, for installing on a PC
#   powershell -File tools/package.ps1 -TestInstall    # ...and register an unpacked test copy for this user
#   powershell -File tools/package.ps1 -Uninstall      # remove that test copy
#
# Upload the .msixupload to Partner Center, which signs the package. Before the first submission
# put the Partner Center identity into app\Windedit\Package.appxmanifest (the PLACEHOLDER values);
# the script stops if they are still there, unless -Sideload or -TestInstall is given, which
# swap in test identities in a copy and leave the upload alone.
#
# The test certificate is created in your user certificate store (CN=Velochrome Test) on first
# use and is never written into the repository. A package signed with it installs only where the
# certificate is trusted (an administrator can import artifacts\store\VelochromeTest.cer into
# "Trusted People"); -TestInstall needs no certificate but Developer Mode (Settings > System >
# For developers), and runs the package under the name Velochrome.Dev so it never collides with
# the Store version.
param([switch]$Sideload, [switch]$TestInstall, [switch]$Uninstall, [switch]$Force)
$ErrorActionPreference = 'Stop'

$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$project = Join-Path $root 'app\Windedit\Windedit.csproj'
$manifestSource = Join-Path $root 'app\Windedit\Package.appxmanifest'
$out = Join-Path $root 'artifacts\store'
$testName = 'Velochrome.Dev'
$testLayout = Join-Path $root 'artifacts\test-install'
$certSubject = 'CN=Velochrome Test'

function Find-SdkTool([string]$name) {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $tool = Get-ChildItem $kits -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName "x64\$name" } | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $tool) { throw "$name not found: install the Windows SDK." }
    $tool
}

function Stop-Velochrome {
    $running = @(Get-Process Velochrome -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { return }
    if (-not $Force) { throw 'Velochrome is running. Close it and run this again (or use -Force).' }
    $running | Stop-Process -Force
    $running | ForEach-Object { $_.WaitForExit(5000) | Out-Null }
}

function Remove-TestCopy {
    $installed = Get-AppxPackage -Name $testName
    if ($installed) {
        Stop-Velochrome
        $installed | Remove-AppxPackage
        "Removed the test copy ($($installed.Version))."
    }
    if (Test-Path $testLayout) { Remove-Item $testLayout -Recurse -Force }
}

function Test-DeveloperMode {
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'
    (Get-ItemProperty $key -Name AllowDevelopmentWithoutDevLicense -ErrorAction SilentlyContinue).AllowDevelopmentWithoutDevLicense -eq 1
}

if ($Uninstall) { Remove-TestCopy; return }
if ($TestInstall -and -not (Test-DeveloperMode)) {
    throw 'Installing a test copy needs Developer Mode: Settings > System > For developers > Developer Mode. Turn it on and run this again.'
}

$placeholders = (Get-Content $manifestSource -Raw) -match 'PLACEHOLDER'
if ($placeholders -and -not ($Sideload -or $TestInstall)) {
    throw 'Package.appxmanifest still has PLACEHOLDER values: fill in the identity from Partner Center first (see the comment at its top).'
}

'Building the package...'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
dotnet publish $project -c Release -r win-x64 -p:Platform=x64 -p:StorePackage=true "-p:AppxPackageDir=$out\" -nologo -v q
if ($LASTEXITCODE -ne 0) { throw "The build failed (exit code $LASTEXITCODE)." }
$built = Get-ChildItem $out -Recurse -Filter 'Windedit_*_x64.msix' | Select-Object -First 1
if (-not $built) { throw "No package was produced in $out." }
$msix = Join-Path $out ($built.Name -replace '^Windedit_', 'Velochrome_')
Move-Item $built.FullName $msix
# Keep just the package; the generated sideloading scripts and runtime copies aren't needed.
Get-ChildItem $out -Directory | Remove-Item -Recurse -Force

# The upload file: a zip holding the package, which is what Partner Center takes.
$upload = [IO.Path]::ChangeExtension($msix, 'msixupload')
$zip = "$upload.zip"
Compress-Archive -Path $msix -DestinationPath $zip -Force
Move-Item $zip $upload -Force
if ($placeholders) {
    'Built {0}; the identity is still the placeholder, so it is not fit to upload.' -f $msix
} else {
    'Built {0} ({1:N1} MB) and {2}. Upload the .msixupload to Partner Center.' -f $msix, ((Get-Item $msix).Length / 1MB), $upload
}

if (-not ($Sideload -or $TestInstall)) { return }

# A copy with the test identity: unpacked, its manifest's name and publisher swapped, packed again.
$makeappx = Find-SdkTool 'makeappx.exe'
$work = Join-Path $root 'artifacts\package-work'
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
& $makeappx unpack /p $msix /d $work /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Unpacking the package failed.' }
$manifestPath = Join-Path $work 'AppxManifest.xml'
[xml]$manifest = Get-Content $manifestPath -Raw
$manifest.Package.Identity.Name = $testName
$manifest.Package.Identity.Publisher = $certSubject
$manifest.Package.Properties.PublisherDisplayName = 'Velochrome Test'
$manifest.Save($manifestPath)

if ($Sideload) {
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $certSubject -and $_.NotAfter -gt (Get-Date).AddDays(30) } | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type Custom -Subject $certSubject -KeyUsage DigitalSignature -FriendlyName 'Velochrome test signing' `
            -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy Exportable -NotAfter (Get-Date).AddYears(2) `
            -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
        'Created the test certificate {0}.' -f $cert.Thumbprint
    }
    $cer = Join-Path $out 'VelochromeTest.cer'
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    $signed = $msix -replace '\.msix$', '_test.msix'
    & $makeappx pack /d $work /p $signed /o | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Packing the test package failed.' }
    & (Find-SdkTool 'signtool.exe') sign /fd SHA256 /sha1 $cert.Thumbprint $signed | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Signing the test package failed.' }
    "Signed test package: $signed"
    "To install it (administrator PowerShell, once for the certificate):"
    "  Import-Certificate -FilePath '$cer' -CertStoreLocation Cert:\LocalMachine\TrustedPeople"
    "  Add-AppxPackage '$signed'"
}

if ($TestInstall) {
    Remove-TestCopy
    Stop-Velochrome
    Copy-Item $work $testLayout -Recurse
    Add-AppxPackage -Register (Join-Path $testLayout 'AppxManifest.xml')
    $installed = Get-AppxPackage -Name $testName
    "Installed the test copy ($($installed.Version)). Start it from the Start menu (Velochrome), or:"
    "  explorer.exe shell:AppsFolder\$($installed.PackageFamilyName)!App"
    "Its private AppData is in $env:LOCALAPPDATA\Packages\$($installed.PackageFamilyName)\LocalCache."
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
