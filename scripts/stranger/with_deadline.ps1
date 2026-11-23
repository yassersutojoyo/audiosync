# Runs a stranger harness script with a deadline, for the jobs in
# .github/workflows/stranger.yml. On 22 and 23 Nov 2026 a hung harness
# outlived the deadline, the step timeout and the job limit four times, log
# lost: the runner's step cancel waits on every process in the step's tree,
# and one stuck in the kernel never dies. So the harness runs detached from
# this shell's tree, and the process list and the kill run in helpers waited
# for with a bound. On a timeout or a failure it prints the harness output,
# the AudioSync debug log tail and the process list and exits non-zero; a
# timeout also sets the step output hung=true.
#
#   pwsh scripts/stranger/with_deadline.ps1 -Minutes <n> -Work <folder> -Script <ps1> [harness args]
# The harness gets -Work <folder> too.
param(
  [Parameter(Mandatory)] [int] $Minutes,
  [Parameter(Mandatory)] [string] $Work,
  [Parameter(Mandatory)] [string] $Script,
  [Parameter(ValueFromRemainingArguments)] [string[]] $Rest = @()
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path
$out = Join-Path $Work 'harness-output.txt'
$done = Join-Path $Work 'harness-exit.txt'
$pidFile = Join-Path $Work 'harness-pid.txt'
Remove-Item $done, $pidFile -ErrorAction SilentlyContinue
$quoted = @(@($Rest) + '-Work' + $Work | ForEach-Object { if ($_ -like '-*') { $_ } else { "'$($_ -replace "'", "''")'" } }) -join ' '
# A throw ends the harness outside its redirection, so the catch writes it.
$command = "`$code = 1; try { & '$((Resolve-Path $Script).Path)' $quoted *> '$out'; `$code = [int] `$LASTEXITCODE } catch { `$_ | Out-File -Append '$out' }; Set-Content '$done' `$code; exit `$code"
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
$shell = (Get-Process -Id $PID).Path

function Start-Hidden([string] $Command) {
  Start-Process -FilePath $shell -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-Command', $Command
}

"deadline $(Get-Date -Format HH:mm:ss): $Script gets $Minutes min"
# An empty RUNNER_TRACKING_ID keeps the runner's end-of-job cleanup off the
# harness too; the kill below is its cleanup.
$launcher = Start-Hidden "`$env:RUNNER_TRACKING_ID = ''; (Start-Process -FilePath '$shell' -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-EncodedCommand', '$encoded').Id | Set-Content '$pidFile'"
if (-not $launcher.WaitForExit(60000) -or -not (Test-Path $pidFile)) { "::error::$Script did not start"; exit 1 }
$harness = [int] (Get-Content $pidFile)
$stop = (Get-Date).AddMinutes($Minutes)
$shown = 0
while (-not (Test-Path $done) -and (Get-Process -Id $harness -ErrorAction SilentlyContinue) -and (Get-Date) -lt $stop) {
  Start-Sleep -Seconds 5
  $lines = @(Get-Content $out -ErrorAction SilentlyContinue)
  if ($lines.Count -gt $shown) { $lines[$shown..($lines.Count - 1)]; $shown = $lines.Count }
}
$lines = @(Get-Content $out -ErrorAction SilentlyContinue)
if ($lines.Count -gt $shown) { $lines[$shown..($lines.Count - 1)] }
$code = Get-Content $done -ErrorAction SilentlyContinue
if ($code -eq '0') { exit 0 }

$hung = -not $code -and (Get-Process -Id $harness -ErrorAction SilentlyContinue)
if ($hung) {
  "::error::$Script still running after $Minutes min; last step: $(@(Get-Content (Join-Path $Work 'result.txt') -ErrorAction SilentlyContinue | Where-Object { $_ -like 'step *' })[-1])"
  if ($env:GITHUB_OUTPUT) { Add-Content -Path $env:GITHUB_OUTPUT -Value 'hung=true' }
} else {
  "::error::$Script failed (exit $(if ($code) { $code } else { 'code not written' }))"
}
foreach ($name in 'env.txt', 'result.txt') {
  "--- $name, last 30 lines"
  Get-Content (Join-Path $Work $name) -Tail 30 -ErrorAction SilentlyContinue
}
"--- AudioSync debug log, last 60 lines"
Get-Content (Join-Path $env:TEMP 'audiosync-debug.log') -Tail 60 -ErrorAction SilentlyContinue
"--- processes"
$list = Join-Path $Work 'processes-at-deadline.txt'
$lister = Start-Hidden "Get-CimInstance Win32_Process | Sort-Object ProcessId | Format-Table -AutoSize ProcessId, ParentProcessId, Name, CommandLine | Out-String -Width 400 | Out-File '$list'"
if (-not $lister.WaitForExit(60000)) { "process list still not written after 60 s" }
Get-Content $list -ErrorAction SilentlyContinue
if ($hung) {
  $killer = Start-Hidden "& taskkill.exe /T /F /PID $harness *> '$(Join-Path $Work 'kill.txt')'"
  if (-not $killer.WaitForExit(30000)) { "taskkill of the harness tree still running after 30 s" }
}
exit 1
