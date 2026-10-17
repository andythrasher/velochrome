# Runs the whole tone measurement: crops the screenshots, develops each raw to scene-linear
# the way the engine does, checks that against the app's default render, and writes one
# quantiles CSV per (folder, app). Everything lands in target/tonefit/.
#
# Run from a PowerShell prompt as `& tools\tonefit\run.ps1`.

$ErrorActionPreference = 'Continue' # failures are caught by the exit-code checks, since cargo writes progress to stderr
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
Set-Location $root
$manifest = Join-Path $root 'slider training photos\manifest.csv'
$out = Join-Path $root 'target\tonefit'
$crops = Join-Path $out 'crops'
$exe = Join-Path $root 'target\release\tonefit.exe'

function Run { & $exe @args; if ($LASTEXITCODE -ne 0) { throw "tonefit $($args[0]) failed" } }

cargo build --release -p tonefit
if ($LASTEXITCODE -ne 0) { throw 'build failed' }
New-Item -ItemType Directory -Force $out | Out-Null

Run extract $manifest $crops | Tee-Object (Join-Path $out 'extract.log')

# One linear image per raw in the manifest (the three in folders 2 and 3, and folder 1's).
$rows = Import-Csv $manifest
$raws = $rows | Select-Object -ExpandProperty raw -Unique
foreach ($raw in $raws) {
    $name = [IO.Path]::GetFileNameWithoutExtension($raw)
    Run linear $raw (Join-Path $out "$name.pfm") 512
}

# Our default render against the app's own default screenshot, for each raw it has one for.
foreach ($row in $rows | Where-Object { $_.app -eq 'windedit' -and $_.slider -eq 'none' }) {
    $name = [IO.Path]::GetFileNameWithoutExtension($row.raw)
    Run check (Join-Path $out "$name.pfm") (Join-Path $crops "$($row.folder)_windedit_none_0.png")
}

# One quantiles CSV per folder and app: all its references against the raw it showed.
foreach ($group in $rows | Group-Object { "$($_.folder)_$($_.app)" }) {
    $first = $group.Group[0]
    $name = [IO.Path]::GetFileNameWithoutExtension($first.raw)
    $csv = Join-Path $out "quantiles_$($group.Name).csv"
    $refs = @($group.Group | ForEach-Object { Join-Path $crops "$($_.folder)_$($_.app)_$($_.slider)_$($_.value).png" })
    Run quantiles (Join-Path $out "$name.pfm") $csv @refs --channels
}
