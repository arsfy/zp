param([int]$ParentProcessId, [string]$Target, [string]$Staged, [string]$Stage)
$ErrorActionPreference = 'Stop'
$process = Get-Process -Id $ParentProcessId -ErrorAction SilentlyContinue
if ($process) { $process.WaitForExit() }
$backup = Join-Path $Stage 'previous.exe'
$ack = Join-Path $Stage 'started'
$started = $false
try {
    Move-Item -LiteralPath $Target -Destination $backup
    try { Move-Item -LiteralPath $Staged -Destination $Target }
    catch { Move-Item -LiteralPath $backup -Destination $Target; throw }
    $env:ZP_UPDATE_ACK = $ack
    $child = Start-Process -FilePath $Target -PassThru
    for ($attempt = 0; $attempt -lt 150; $attempt++) {
        if (Test-Path -LiteralPath $ack) { $started = $true; break }
        if ($child.HasExited) { break }
        Start-Sleep -Milliseconds 200
    }
    if (!$started) {
        if (!$child.HasExited) { Stop-Process -Id $child.Id; $child.WaitForExit() }
        Move-Item -LiteralPath $Target -Destination (Join-Path $Stage 'failed.exe')
        Move-Item -LiteralPath $backup -Destination $Target
        Remove-Item Env:ZP_UPDATE_ACK
        $env:ZP_UPDATE_ROLLBACK = $Stage
        Start-Process -FilePath $Target
        exit 1
    }
} catch {
    $_ | Out-File -LiteralPath (Join-Path $Stage 'error.log')
    if (!(Test-Path -LiteralPath $Target) -and (Test-Path -LiteralPath $backup)) {
        Move-Item -LiteralPath $backup -Destination $Target
    }
    if (Test-Path -LiteralPath $Target) {
        $env:ZP_UPDATE_ROLLBACK = $Stage
        Remove-Item Env:ZP_UPDATE_ACK -ErrorAction SilentlyContinue
        Start-Process -FilePath $Target
    }
    exit 1
}
# The new version is running: cleanup is best effort and never triggers a rollback.
Remove-Item -LiteralPath $Stage -Recurse -Force -ErrorAction SilentlyContinue
exit 0
