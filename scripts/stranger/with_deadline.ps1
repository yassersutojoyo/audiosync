# Runs a stranger harness script with a deadline, for the jobs in
# .github/workflows/stranger.yml. The harness runs in its own hidden window
# with its output in a file, so a child stuck in the kernel holds none of the
# step's pipes: twice on 23 Nov 2026 such a hang kept the step open past its
# own timeout until the job limit, and the job log was lost. On a timeout or
# a failure it prints the harness output, the AudioSync debug log tail and
# the process list, then exits non-zero.
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
$quoted = @(@($Rest) + '-Work' + $Work | ForEach-Object { if ($_ -like '-*') { $_ } else { "'$($_ -replace "'", "''")'" } }) -join ' '
# A throw ends the harness outside its redirection, so the catch writes it.
$command = "try { & '$((Resolve-Path $Script).Path)' $quoted *> '$out' } catch { `$_ | Out-File -Append '$out'; exit 1 }; exit `$LASTEXITCODE"

"deadline $(Get-Date -Format HH:mm:ss): $Script gets $Minutes min"
$child = Start-Process -FilePath (Get-Process -Id $PID).Path -WindowStyle Hidden -PassThru -ArgumentList '-NoProfile', '-Command', $command
$null = $child.Handle
$stop = (Get-Date).AddMinutes($Minutes)
$shown = 0
while (-not $child.HasExited -and (Get-Date) -lt $stop) {
  Start-Sleep -Seconds 5
  $lines = @(Get-Content $out -ErrorAction SilentlyContinue)
  if ($lines.Count -gt $shown) { $lines[$shown..($lines.Count - 1)]; $shown = $lines.Count }
}
$lines = @(Get-Content $out -ErrorAction SilentlyContinue)
if ($lines.Count -gt $shown) { $lines[$shown..($lines.Count - 1)] }
if ($child.HasExited -and $child.ExitCode -eq 0) { exit 0 }

if ($child.HasExited) {
  "::error::$Script failed (exit $($child.ExitCode))"
} else {
  "::error::$Script still running after $Minutes min; last step: $(@(Get-Content (Join-Path $Work 'result.txt') -ErrorAction SilentlyContinue | Where-Object { $_ -like 'step *' })[-1])"
}
foreach ($name in 'env.txt', 'result.txt') {
  "--- $name, last 30 lines"
  Get-Content (Join-Path $Work $name) -Tail 30 -ErrorAction SilentlyContinue
}
"--- AudioSync debug log, last 60 lines"
Get-Content (Join-Path $env:TEMP 'audiosync-debug.log') -Tail 60 -ErrorAction SilentlyContinue
"--- processes"
Get-CimInstance Win32_Process | Sort-Object ProcessId |
  Format-Table -AutoSize ProcessId, ParentProcessId, Name, CommandLine | Out-String -Width 400 |
  Tee-Object -FilePath (Join-Path $Work 'processes-at-deadline.txt')
if (-not $child.HasExited) { & taskkill.exe /T /F /PID $child.Id 2>&1 | Out-Null }
exit 1
