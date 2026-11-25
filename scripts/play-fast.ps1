# Starts the release build with this fork's frame-rate switches.
#   -Exe        the title's executable, from your own copy
#   -Safe       none of the switches
#   -SmallCard  without the two switches that want 16 GB of card memory
# Save your other work first: see "Known risks" in the README.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [switch]$Safe,
    [switch]$SmallCard
)
$root = Split-Path -Parent $PSScriptRoot
$victory = Join-Path $root 'target\release\victory.exe'
if (-not (Test-Path -LiteralPath $victory)) { Write-Error 'Build it first: cargo build --release -p victory'; exit 1 }
if (-not (Test-Path -LiteralPath $Exe)) { Write-Error "Not found: $Exe"; exit 1 }
if (-not $Safe) {
    $env:VICTORY_INDIRECT_DIRECT = '1'
    $env:VICTORY_COPY_FETCH = '1'
    $env:VICTORY_SAFE_COPY = '1'
    $env:VICTORY_FILL_FREE = '1'
    $env:VICTORY_WRITE_BESIDE = '1'
    $env:VICTORY_SKIP_SAME = '1'
    if (-not $SmallCard) {
        $env:VICTORY_CARD_WINDOWS_MB = '6144'
        $env:VICTORY_CARD_BUFFERS_MB = '4096'
    }
}
Set-Location -LiteralPath $root
& $victory run $Exe --execute
exit $LASTEXITCODE
