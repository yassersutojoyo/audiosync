# Posts a stranger harness's progress to a check run every 15 s, started
# detached by with_deadline.ps1. Five default output runs on 22 and 23 Nov
# 2026 lost the whole runner (no step timeout fired, the log was never
# uploaded); the check run keeps the last beat off the runner, and the
# stranger.yml job after it prints that beat and closes the check run.
#
#   pwsh scripts/stranger/heartbeat.ps1 -Work <folder> -Name <check run name> -Minutes <n>
# Needs GH_TOKEN with checks: write, GITHUB_REPOSITORY and GITHUB_SHA.
param(
  [Parameter(Mandatory)] [string] $Work,
  [Parameter(Mandatory)] [string] $Name,
  [Parameter(Mandatory)] [int] $Minutes
)
$ErrorActionPreference = 'Continue'
$api = "repos/$env:GITHUB_REPOSITORY/check-runs"
$body = Join-Path $Work 'heartbeat.json'
$done = Join-Path $Work 'harness-exit.txt'

# A gh call that does not answer in 20 s is dropped; the next beat tries again.
function Send([string] $Path, [string] $Method, [hashtable] $Fields) {
  $Fields | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8NoBOM $body
  $p = Start-Process -FilePath gh -NoNewWindow -PassThru -ArgumentList 'api', '-X', $Method, $Path, '--input', "`"$body`"", '--jq', '.id' `
    -RedirectStandardOutput "$body.out" -RedirectStandardError "$body.err"
  $null = $p.Handle
  if (-not $p.WaitForExit(20000)) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue; return $null }
  if ($p.ExitCode -eq 0) { Get-Content "$body.out" -ErrorAction SilentlyContinue }
}

$id = Send $api POST @{ name = $Name; head_sha = $env:GITHUB_SHA; status = 'in_progress'; output = @{ title = 'started'; summary = 'no beat yet' } }
if ("$id" -notmatch '^\d+$') { "heartbeat: no check run ($(Get-Content "$body.err" -ErrorAction SilentlyContinue))" | Set-Content (Join-Path $Work 'heartbeat-error.txt'); exit 0 }
$stop = (Get-Date).AddMinutes($Minutes)
$beat = 0
while ($true) {
  $finished = Test-Path $done
  $beat++
  $harness = Get-Content (Join-Path $Work 'harness-pid.txt') -ErrorAction SilentlyContinue
  $alive = $harness -and (Get-Process -Id ([int] $harness) -ErrorAction SilentlyContinue)
  $poll = Get-Content (Join-Path $Work 'deadline-poll.txt') -ErrorAction SilentlyContinue
  $services = (Get-Service Audiosrv, AudioEndpointBuilder -ErrorAction SilentlyContinue | ForEach-Object { "$($_.Name) $($_.Status)" }) -join ', '
  $tail = (Get-Content (Join-Path $Work 'harness-output.txt') -Tail 40 -ErrorAction SilentlyContinue) -join "`n"
  $fields = @{ output = @{
      title = "beat $beat at $((Get-Date).ToUniversalTime().ToString('HH:mm:ss'))Z"
      summary = "harness $(if ($alive) { 'running' } else { 'not running' }); step shell last polled $poll; $services"
      text = "~~~`n$tail`n~~~"
    } }
  if ($finished) { $fields.status = 'completed'; $fields.conclusion = if ("$(Get-Content $done)" -eq '0') { 'success' } else { 'failure' } }
  $null = Send "$api/$id" PATCH $fields
  if ($finished -or (Get-Date) -gt $stop) { exit 0 }
  Start-Sleep -Seconds 15
}
