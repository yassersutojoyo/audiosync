# Runner only (call-mic-probe.yml): Zoom Workplace and Microsoft Teams
# desktop, installed through winget, each driven through UI Automation to a
# screen that needs no account, while `call_mic_probe callapp` reads where
# the app's output lands (S485, S486). Signs in nowhere and creates no
# account; a step that would need one is logged as untested.
param([string] $Probe, [string] $Log, [int] $Runs = 5)

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$dir = Split-Path $Log

function Say([string] $Line) { $Line | Tee-Object -FilePath $Log -Append }

function Save-Screen([string] $Name) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $shot = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen($b.X, $b.Y, 0, 0, $shot.Size)
  $shot.Save((Join-Path $dir "$Name.png"))
}

function Get-AppElements([string[]] $Exes) {
  $ids = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $Exes -contains "$($_.ProcessName).exe".ToLower() } | ForEach-Object Id)
  foreach ($top in $A::RootElement.FindAll($Scope::Children, [System.Windows.Automation.Condition]::TrueCondition)) {
    if ($ids -contains $top.Current.ProcessId) {
      $top
      $top.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    }
  }
}

function Save-Tree([string[]] $Exes, [string] $Name) {
  Get-AppElements $Exes | ForEach-Object {
    "$($_.Current.ControlType.ProgrammaticName) | $($_.Current.Name) | $($_.Current.AutomationId) | enabled=$($_.Current.IsEnabled)"
  } | Out-File -Encoding utf8 (Join-Path $dir "$Name-tree.txt")
  Save-Screen $Name
}

function Invoke-Element($Element) {
  $p = $null
  foreach ($pattern in [System.Windows.Automation.InvokePattern]::Pattern, [System.Windows.Automation.SelectionItemPattern]::Pattern,
    [System.Windows.Automation.TogglePattern]::Pattern) {
    if ($Element.TryGetCurrentPattern($pattern, [ref]$p)) {
      switch ($pattern.ProgrammaticName) {
        { $_ -like '*Invoke*' } { $p.Invoke() }
        { $_ -like '*SelectionItem*' } { $p.Select() }
        default { $p.Toggle() }
      }
      return $true
    }
  }
  $false
}

# Invokes, in order and once each, the first enabled element whose name
# matches each pattern, waiting up to $Seconds for it; names never matched
# go to the log.
function Use-Steps([string[]] $Exes, [string[]] $Steps, [int] $Seconds, [string] $Run) {
  foreach ($step in $Steps) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    $done = $false
    while (-not $done -and (Get-Date) -lt $deadline) {
      $hit = Get-AppElements $Exes | Where-Object { $_.Current.IsEnabled -and $_.Current.Name -match $step } | Select-Object -First 1
      if ($hit) { $done = Invoke-Element $hit; Say "CALLAPP $Run step '$step': '$($hit.Current.Name)' invoked=$done" }
      else { Start-Sleep -Milliseconds 500 }
    }
    if (-not $done) { Say "CALLAPP $Run step '$step': not found in $Seconds s" }
    Start-Sleep -Seconds 3
  }

}

function Install-App([string] $Id) {
  $out = winget install --id $Id -e --silent --disable-interactivity --accept-package-agreements --accept-source-agreements 2>&1
  Say "CALLAPP winget install $Id exit $LASTEXITCODE"
  $out | Out-File -Append $Log
  winget list --id $Id -e --accept-source-agreements 2>&1 | Out-File -Append $Log
}

function Start-Probe([string] $Label, [string] $Exes, [int] $Seconds) {
  $out = Join-Path $dir "$Label.txt"
  $p = Start-Process -FilePath $Probe -ArgumentList 'callapp', $Label, $Exes, $Seconds -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError "$out.err"
  Start-Sleep -Seconds 2
  @{ Process = $p; Out = $out }
}

function Stop-Probe($Started) {
  $Started.Process.WaitForExit(120000) | Out-Null
  Get-Content $Started.Out, "$($Started.Out).err" -ErrorAction SilentlyContinue | Tee-Object -FilePath $Log -Append
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
  Say 'CALLAPP winget missing; installing it through Microsoft.WinGet.Client'
  Install-PackageProvider -Name NuGet -Force -Scope AllUsers | Out-Null
  Install-Module Microsoft.WinGet.Client -Force -Scope AllUsers -Repository PSGallery
  Repair-WinGetPackageManager -AllUsers -Latest
  $env:Path += ";$env:LOCALAPPDATA\Microsoft\WindowsApps"
}
Say "CALLAPP winget $(winget --version 2>&1)"
Say "CALLAPP $((Get-CimInstance Win32_OperatingSystem).Caption) build $([Environment]::OSVersion.Version)"

# Zoom: the test meeting (zoom.us/test) needs no account; its join link is
# read from the page, and the audio prompt's buttons are the call path.
Install-App 'Zoom.Zoom'
$zoom = @("$env:ProgramFiles\Zoom\bin\Zoom.exe", "$env:APPDATA\Zoom\bin\Zoom.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $zoom) {
  Say 'CALLAPP-RESULT zoom untested: Zoom.exe not found after the install'
} else {
  Say "CALLAPP zoom version $((Get-Item $zoom).VersionInfo.ProductVersion) at $zoom"
  $page = try { (Invoke-WebRequest -UseBasicParsing -Uri 'https://zoom.us/test').Content } catch { "$_" }
  $page | Out-File -Encoding utf8 (Join-Path $dir 'zoom-test-page.html')
  $confno = [regex]::Match($page, '(?:confno=|/j/)(\d{9,11})').Groups[1].Value
  Say "CALLAPP zoom test meeting number '$confno'"
  $steps = '^Join$', 'Join with Computer Audio|Join Audio by Computer|Join audio', 'Test Speaker and Microphone|Test Speaker|Test speaker', 'Test Mic|Test mic'
  foreach ($run in 1..$Runs) {
    Get-Process Zoom -ErrorAction SilentlyContinue | Stop-Process -Force
    $started = Start-Probe "zoom-$run" 'zoom.exe' 75
    $url = if ($confno) { "zoommtg://zoom.us/join?action=join&confno=$confno&uname=probe" } else { '' }
    $zoomArgs = if ($url) { @("--url=$url") } else { @() }
    Say "CALLAPP zoom run $run command line: `"$zoom`" $($zoomArgs -join ' ')"
    if ($zoomArgs) { Start-Process -FilePath $zoom -ArgumentList $zoomArgs } else { Start-Process -FilePath $zoom }
    Start-Sleep -Seconds 15
    if ($run -eq 1) { Save-Tree @('zoom.exe') 'zoom-start' }
    $null = Use-Steps @('zoom.exe') $steps 12 "zoom run $run"
    if ($run -eq 1) { Save-Tree @('zoom.exe') 'zoom-after' }
    Stop-Probe $started
  }
  Get-Process Zoom -ErrorAction SilentlyContinue | Stop-Process -Force
}

# Teams: with no account only its start screen is reachable; whatever it
# offers there is driven, and the sign-in is never pressed.
Install-App 'Microsoft.Teams'
$teams = Get-AppxPackage -AllUsers -Name 'MSTeams' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $teams) {
  Say 'CALLAPP-RESULT teams untested: the MSTeams package is not installed'
} else {
  Say "CALLAPP teams version $($teams.Version)"
  $exes = @('ms-teams.exe', 'msedgewebview2.exe')
  foreach ($run in 1..$Runs) {
    Get-Process ms-teams -ErrorAction SilentlyContinue | Stop-Process -Force
    $started = Start-Probe "teams-$run" ($exes -join ',') 45
    Say "CALLAPP teams run $run command line: explorer.exe shell:AppsFolder\$($teams.PackageFamilyName)!MSTeams"
    Start-Process explorer.exe "shell:AppsFolder\$($teams.PackageFamilyName)!MSTeams"
    Start-Sleep -Seconds 25
    if ($run -eq 1) { Save-Tree $exes 'teams-start' }
    $null = Use-Steps $exes @('Settings|Devices', 'Make a test call|Test') 8 "teams run $run"
    Stop-Probe $started
  }
  Get-Process ms-teams -ErrorAction SilentlyContinue | Stop-Process -Force
}
Select-String -Path $Log -Pattern '^CALLAPP-RESULT' | Group-Object { ($_.Line -split ' ')[1] -replace '-\d+$', '' } |
  ForEach-Object { Say "CALLAPP-SUMMARY $($_.Name): $(@($_.Group | Where-Object { $_.Line -match ' pass:' }).Count)/$($_.Count) pass; $(($_.Group | ForEach-Object { ($_.Line -split ' ', 3)[2] }) -join '; ')" }
