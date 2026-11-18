# The installed app's screens on a clean Windows runner, run by
# .github/workflows/stranger.yml: a silent install of AudioSync-Setup.exe, then
# per theme a launch, one screen group driven through UI Automation only
# (Invoke, Toggle and ExpandCollapse patterns), and each state photographed as
# <STATE>-<terang|gelap>.png, the names of the Mac's LAYAR photos where a state
# matches. A state the runner can't reach goes to not-set-up.txt as
# screen_compare.py reads it; uia-<theme>.txt holds the tree as UIA saw it.
#
#   pwsh scripts/stranger/windows_shots.ps1 -Setup <AudioSync-Setup.exe> -Work <folder> -Group <main|delay|tray|calibrate|notices>
param(
  [Parameter(Mandatory)] [string] $Setup,
  [Parameter(Mandatory)] [string] $Work,
  [Parameter(Mandatory)] [ValidateSet('main', 'delay', 'tray', 'calibrate', 'notices')] [string] $Group
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$script:missing = @()

$install = Start-Process -FilePath $Setup -Wait -PassThru `
  -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$Work\install.log`""
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
if ($install.ExitCode -ne 0 -or -not (Test-Path $exe)) { throw "install failed (exit $($install.ExitCode))" }
"installed: $exe"

function Get-AppWindows {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $script:app.Id)
  @($A::RootElement.FindAll($Scope::Children, $c))
}

# The first element of the app named exactly $Name, in any of its windows.
function Find-Named([string] $Name, [int] $Seconds = 10) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $Name)
  $deadline = (Get-Date).AddSeconds($Seconds)
  do {
    $windows = Get-AppWindows
    foreach ($w in $windows) { if ($w.Current.Name -eq $Name) { return $w } }
    foreach ($w in $windows) {
      $e = $w.FindFirst($Scope::Descendants, $c)
      if ($e) { return $e }
    }
    Start-Sleep -Milliseconds 500
  } while ((Get-Date) -lt $deadline)
  $null
}

function Invoke-Element($Element) {
  $p = $null
  if ($Element.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$p)) { $p.Invoke(); return }
  if ($Element.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$p)) { $p.Toggle(); return }
  if ($Element.TryGetCurrentPattern([System.Windows.Automation.ExpandCollapsePattern]::Pattern, [ref]$p)) {
    if ($p.Current.ExpandCollapseState -eq 'Collapsed') { $p.Expand() } else { $p.Collapse() }
    return
  }
  throw "no Invoke, Toggle or ExpandCollapse pattern on '$($Element.Current.Name)'"
}

# Invoke the element named $Name; false and a not-set-up line for $State when it isn't there.
function Use-Named([string] $Name, [string] $State) {
  $e = Find-Named $Name
  if (-not $e) { Skip-State $State "no element named '$Name' (see uia-$script:theme.txt)"; return $false }
  try { Invoke-Element $e } catch { Skip-State $State $_.Exception.Message; return $false }
  Start-Sleep -Seconds 2
  $true
}

function Skip-State([string] $State, [string] $Why) {
  Write-Host "not reached: $State $script:theme ($Why)"
  $script:missing += "$State ${script:theme}: $Why"
}

function Save-Shot($Window, [string] $State) {
  $r = if ($Window) { $Window.Current.BoundingRectangle } else { [System.Windows.Rect]::Empty }
  if ($r.IsEmpty -or $r.Width -lt 1) {
    $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $r = New-Object System.Windows.Rect($b.X, $b.Y, $b.Width, $b.Height)
  }
  $shot = New-Object System.Drawing.Bitmap ([int]$r.Width), ([int]$r.Height)
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen([int]$r.X, [int]$r.Y, 0, 0, $shot.Size)
  $shot.Save((Join-Path $Work "$State-$script:theme.png"), [System.Drawing.Imaging.ImageFormat]::Png)
  "shot: $State-$script:theme.png ($([int]$r.Width)x$([int]$r.Height))"
}

function Save-Tree {
  $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
  $lines = New-Object System.Collections.Generic.List[string]
  function Walk($e, [int] $depth) {
    if ($depth -gt 14) { return }
    $lines.Add(('  ' * $depth) + "$($e.Current.ControlType.ProgrammaticName) '$($e.Current.Name)' $($e.Current.BoundingRectangle)")
    $child = $walker.GetFirstChild($e)
    while ($child) { Walk $child ($depth + 1); $child = $walker.GetNextSibling($child) }
  }
  foreach ($w in Get-AppWindows) { Walk $w 0 }
  $lines | Set-Content (Join-Path $Work "uia-$script:theme.txt")
}

# The notification-area button whose name holds AudioSync, promoted out of the
# hidden icons first (Windows 11 keeps a new icon there).
function Find-TrayButton {
  Get-ChildItem 'HKCU:\Control Panel\NotifyIconSettings' -ErrorAction SilentlyContinue |
    Where-Object { (Get-ItemProperty $_.PSPath).ExecutablePath -like '*audiosync.exe' } |
    ForEach-Object { Set-ItemProperty $_.PSPath -Name IsPromoted -Value 1 -Type DWord }
  Start-Sleep -Seconds 2
  $button = New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, [System.Windows.Automation.ControlType]::Button)
  foreach ($pass in 1, 2) {
    foreach ($top in $A::RootElement.FindAll($Scope::Children, [System.Windows.Automation.Condition]::TrueCondition)) {
      if ($top.Current.ClassName -notmatch 'Shell_TrayWnd|NotifyIconOverflowWindow|TopLevelWindowForOverflowXamlIsland') { continue }
      foreach ($b in $top.FindAll($Scope::Descendants, $button)) {
        if ($b.Current.Name -like '*AudioSync*') { return $b }
        if ($pass -eq 1 -and $b.Current.Name -match 'hidden icons|Notification Chevron') { $chevron = $b }
      }
    }
    if ($chevron) { Invoke-Element $chevron; Start-Sleep -Seconds 2 } else { break }
  }
  $null
}

foreach ($script:theme in 'terang', 'gelap') {
  $data = Join-Path $env:LOCALAPPDATA 'AudioSync'
  New-Item -ItemType Directory -Force -Path $data | Out-Null
  # Settings > Appearance, as the user would leave it; everything else at its default.
  $appearance = if ($script:theme -eq 'gelap') { 'dark' } else { 'light' }
  "{`"version`":3,`"appearance`":`"$appearance`"}" | Set-Content -Encoding ascii (Join-Path $data 'settings.json')

  $env:AUDIOSYNC_DEBUG = '1'
  $script:app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
  Remove-Item Env:AUDIOSYNC_DEBUG
  $main = $null
  for ($i = 0; $i -lt 30 -and -not $main; $i++) { Start-Sleep -Seconds 1; $main = @(Get-AppWindows)[0] }
  Start-Sleep -Seconds 3
  if (-not $main) { Skip-State 'MAIN-IDLE' 'no window within 30 s' }
  else {
    try { $main.SetFocus() } catch { "focus: $($_.Exception.Message)" }
    switch ($Group) {
      'main' {
        Save-Shot $main 'MAIN-IDLE'
        if (Use-Named 'Presets' 'MAIN-PRESETS-EMPTY') { Save-Shot $main 'MAIN-PRESETS-EMPTY'; Use-Named 'Presets' 'MAIN-PRESETS-EMPTY' | Out-Null }
        if ((Use-Named 'Options' 'MAIN-SETTINGS') -and (Use-Named "Settings$([char]0x2026)" 'MAIN-SETTINGS')) {
          $settings = Find-Named 'Settings'
          if ($settings) { Save-Shot $settings 'MAIN-SETTINGS' } else { Skip-State 'MAIN-SETTINGS' 'no Settings window' }
        }
      }
      'delay' {
        Save-Tree
        Skip-State 'MAIN-DELAY-ROWS' 'the runner has no audio output endpoint, so This PC lists no speaker to turn on'
      }
      'tray' {
        $tray = Find-TrayButton
        if (-not $tray) { Skip-State 'TRAY-IDLE' 'no AudioSync button in the notification area' }
        else {
          Invoke-Element $tray
          Start-Sleep -Seconds 2
          $flyout = Get-AppWindows | Where-Object { $_.Current.NativeWindowHandle -ne $main.Current.NativeWindowHandle } | Select-Object -First 1
          if ($flyout) { Save-Shot $flyout 'TRAY-IDLE' } else { Skip-State 'TRAY-IDLE' 'the tray button gave no panel' }
        }
      }
      'calibrate' {
        # No speaker or microphone on the runner: the sheet shows what a run without them says.
        if (Use-Named 'Calibrate All Speakers' 'CAL-SHEET') {
          Save-Shot $main 'CAL-SHEET'
          Start-Sleep -Seconds 15
          Save-Shot $main 'CAL-SHEET-LATER'
        }
      }
      'notices' {
        # Streaming on with no speaker on: the No Speaker notice.
        if (Use-Named 'Streaming' 'NOTICE-START') { Start-Sleep -Seconds 2; Save-Shot $main 'NOTICE-START' }
      }
    }
    Save-Tree
  }
  if (-not $script:app.HasExited) { Stop-Process -Id $script:app.Id -Force }
  Start-Sleep -Seconds 2
  $log = Join-Path $env:TEMP 'audiosync-debug.log'
  if (Test-Path $log) { Move-Item -Force $log (Join-Path $Work "audiosync-debug-$script:theme.log") }
}

$script:missing | Set-Content (Join-Path $Work 'not-set-up.txt')
$shots = @(Get-ChildItem $Work -Filter '*.png').Count
"$shots pictures, $($script:missing.Count) states not reached"
if ($shots -eq 0) { exit 1 }
