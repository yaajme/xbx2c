# Starts the release build with this fork's frame-rate switches.
#   -Exe           the title's executable, from your own copy
#   -Safe          none of the switches
#   -SmallCard     without the two switches that want 16 GB of card memory
#   -Experimental  also the newest switches (see "Newer" in the README)
#   -Ambient 1     an estimate in place of the missing indirect light (a share of
#                  it: 1 all; constant:0.6 the flat light of earlier versions)
#   -Log <file>    the log goes to this file
# Save your other work first: see "Known risks" in the README.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [switch]$Safe,
    [switch]$SmallCard,
    [switch]$Experimental,
    [string]$Ambient,
    [string]$Log
)
$root = Split-Path -Parent $PSScriptRoot
$victory = Join-Path $root 'target\release\victory.exe'
if (-not (Test-Path -LiteralPath $victory)) { Write-Error 'Build it first: cargo build --release -p victory'; exit 1 }
if (-not (Test-Path -LiteralPath $Exe)) { Write-Error "Not found: $Exe"; exit 1 }
if ($Safe -and ($Experimental -or $Ambient)) { Write-Error '-Safe goes with no other switch'; exit 1 }
$Exe = (Resolve-Path -LiteralPath $Exe).Path
if (-not $Safe) {
    $env:VICTORY_INDIRECT_DIRECT = '1'
    $env:VICTORY_QUEUE2 = '1'
    $env:VICTORY_SAFE_COPY = '1'
    $env:VICTORY_WRITE_BESIDE = '1'
    $env:VICTORY_SKIP_SAME = '1'
    if (-not $SmallCard) {
        $env:VICTORY_CARD_WINDOWS_MB = '6144'
        $env:VICTORY_CARD_BUFFERS_MB = '4096'
    }
    if ($Experimental) {
        $env:VICTORY_ASYNC_SUBMIT = '1'
        $env:VICTORY_MERGE_FINE = '1'
        $env:VICTORY_CARD_HASH_WIDE = '1'
        $env:VICTORY_THIN = '1'
        $env:VICTORY_CARD_REHASH_RUNS = '1'
        $env:VICTORY_GUARD_ONCE = '1'
        $env:VICTORY_HEAP_SYNC_PAGES = '1'
        $env:VICTORY_SYNC_ASIDE = '1'
        $env:VICTORY_WIDE_REFRESH = '1'
        $env:VICTORY_BURY_THREAD = '1'
        $env:VICTORY_TEXTURE_NO_WAIT = '1'
    }
    if ($Ambient) { $env:VICTORY_AMBIENT = $Ambient }
}
Set-Location -LiteralPath $root
if ($Log) {
    $Log = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Log)
    cmd /c "`"`"$victory`" run `"$Exe`" --execute > `"$Log`" 2>&1`""
} else {
    & $victory run $Exe --execute
}
exit $LASTEXITCODE
