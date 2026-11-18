# The app's keyboard shortcuts pressed for real on a clean Windows runner, run
# by .github/workflows/stranger.yml: a silent install, then per window (main,
# Settings, Equalizer, Guide, About, Web Remote) every Ctrl shortcut the gear
# menu lists, sent with SendInput so Windows holds the Ctrl key as a person's
# keyboard does (posted messages carry no modifier). Each effect is read back
# through UI Automation or the debug log; Escape is pressed in the zone dialog,
# the PIN dialog and the Calibrate sheet. results.txt holds one line per key.
#
#   pwsh scripts/stranger/windows_keys.ps1 -Setup <AudioSync-Setup.exe> -Work <folder>
param(
  [Parameter(Mandatory)] [string] $Setup,
  [Parameter(Mandatory)] [string] $Work
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Keys {
  [StructLayout(LayoutKind.Sequential)] struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Explicit)] struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public INPUTUNION u; }
  [DllImport("user32.dll", SetLastError = true)] static extern uint SendInput(uint n, INPUT[] inputs, int size);
  [DllImport("user32.dll")] static extern uint MapVirtualKey(uint code, uint mapType);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
  [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr hwnd);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, IntPtr pid);
  [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] static extern bool AttachThreadInput(uint from, uint to, bool attach);

  static INPUT Key(ushort vk, bool up) {
    var i = new INPUT { type = 1 };
    i.u.ki.wVk = vk;
    i.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
    i.u.ki.dwFlags = up ? 2u : 0u;
    return i;
  }

  // vk pressed while the keys in mods are held, as one SendInput batch.
  public static uint Press(ushort[] mods, ushort vk) {
    var list = new System.Collections.Generic.List<INPUT>();
    foreach (var m in mods) list.Add(Key(m, false));
    list.Add(Key(vk, false));
    list.Add(Key(vk, true));
    for (int k = mods.Length - 1; k >= 0; k--) list.Add(Key(mods[k], true));
    return SendInput((uint)list.Count, list.ToArray(), Marshal.SizeOf(typeof(INPUT)));
  }

  // The foreground lock lets a background process take the foreground only
  // through the input queue of the window that has it.
  public static bool Front(IntPtr hwnd) {
    uint them = GetWindowThreadProcessId(GetForegroundWindow(), IntPtr.Zero);
    uint me = GetCurrentThreadId();
    if (them != me) AttachThreadInput(me, them, true);
    BringWindowToTop(hwnd);
    bool ok = SetForegroundWindow(hwnd);
    if (them != me) AttachThreadInput(me, them, false);
    return ok;
  }
}
'@
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$VkControl = [uint16]0x11
$VK = @{ 'w' = 0x57; 'q' = 0x51; 'n' = 0x4E; 'e' = 0x45; 'r' = 0x52; ',' = 0xBC; '0' = 0x30; '1' = 0x31; '2' = 0x32
  'Escape' = 0x1B; 'F1' = 0x70; 'F5' = 0x74 }
$script:results = New-Object System.Collections.Generic.List[string]
$script:failed = 0
$refreshLine = 'the user asked to list the devices again'
$log = Join-Path $env:TEMP 'audiosync-debug.log'

$install = Start-Process -FilePath $Setup -Wait -PassThru `
  -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$Work\install.log`""
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
if ($install.ExitCode -ne 0 -or -not (Test-Path $exe)) { throw "install failed (exit $($install.ExitCode))" }

# Two presets whose only speaker is absent: Ctrl+1 and Ctrl+2 recall them, and
# the debug log names the one recalled.
$data = Join-Path $env:LOCALAPPDATA 'AudioSync'
New-Item -ItemType Directory -Force -Path $data | Out-Null
$preset = '{{"id":"keys-{0}","name":"Keys {1}","members":[{{"key":"lc:absent-{0}","volume":0.5}}],"zones":[],"master_volume":50,"gains_db":[0,0,0,0,0,0,0,0,0,0],"reference_member":true}}'
$settingsJson = '{"version":3,"appearance":"light","speaker_presets_migrated":true,"speaker_presets":[' +
  ($preset -f 1, 'One') + ',' + ($preset -f 2, 'Two') + ']}'

function Record([string] $Window, [string] $Key, [bool] $Ok, [string] $What) {
  $line = "$(if ($Ok) { 'PASS' } else { 'FAIL' }) | $Window | $Key | $What"
  Write-Host $line
  $script:results.Add($line)
  if (-not $Ok) { $script:failed++ }
}

function Wait-Until([scriptblock] $Test, [int] $Seconds = 6) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  do { if (& $Test) { return $true }; Start-Sleep -Milliseconds 250 } while ((Get-Date) -lt $deadline)
  $false
}

function Get-AppWindows {
  if ($script:app.HasExited) { return @() }
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $script:app.Id)
  @($A::RootElement.FindAll($Scope::Children, $c) | Where-Object { [Keys]::IsWindowVisible([IntPtr]$_.Current.NativeWindowHandle) })
}

# The app's shown window titled $Title other than the main window (the zone
# dialog's title is the app name too).
function Find-Window([string] $Title) {
  Get-AppWindows | Where-Object { $_.Current.Name -eq $Title -and $_.Current.NativeWindowHandle -ne $script:mainHandle } |
    Select-Object -First 1
}

function Main-Shown { [Keys]::IsWindowVisible([IntPtr]$script:mainHandle) }

function Find-Named([string] $Name) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $Name)
  foreach ($w in Get-AppWindows) {
    foreach ($e in $w.FindAll($Scope::Descendants, $c)) {
      $p = $null
      if ($e.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$p)) { return $e }
    }
  }
  $null
}

function Invoke-Named([string] $Name) {
  if (-not (Wait-Until { ($script:e = Find-Named $Name) -ne $null } 8)) { return $false }
  $script:e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
  Start-Sleep -Seconds 1
  $true
}

function Focus-Window($Element) {
  if (-not $Element) { Write-Host 'focus: no window to focus'; return }
  $h = [IntPtr]$Element.Current.NativeWindowHandle
  [void][Keys]::Front($h)
  try { $Element.SetFocus() } catch { }
  Wait-Until { [Keys]::GetForegroundWindow() -eq $h } 3 | Out-Null
  Start-Sleep -Milliseconds 400
}

function Press([string] $Key, [switch] $Ctrl) {
  # Not an if expression: one that yields an empty array yields $null.
  [uint16[]] $mods = @()
  if ($Ctrl) { $mods = @($VkControl) }
  $sent = [Keys]::Press($mods, [uint16]$VK[$Key])
  if ($sent -eq 0) { Write-Host "SendInput sent nothing for $Key" }
  Start-Sleep -Milliseconds 600
}

function Log-Count([string] $Text) {
  if (-not (Test-Path $log)) { return 0 }
  @(Select-String -Path $log -SimpleMatch $Text -ErrorAction SilentlyContinue).Count
}

function Save-Shot($Window, [string] $Name) {
  $r = $Window.Current.BoundingRectangle
  if ($r.IsEmpty -or $r.Width -lt 1) { return }
  $shot = New-Object System.Drawing.Bitmap ([int]$r.Width), ([int]$r.Height)
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen([int]$r.X, [int]$r.Y, 0, 0, $shot.Size)
  $shot.Save((Join-Path $Work "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
}

function Start-App([hashtable] $Extra = @{}) {
  Remove-Item -Force -ErrorAction SilentlyContinue $log
  $settingsJson | Set-Content -Encoding ascii (Join-Path $data 'settings.json')
  $env:AUDIOSYNC_DEBUG = '1'
  foreach ($k in $Extra.Keys) { Set-Item "Env:$k" $Extra[$k] }
  $script:app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
  Remove-Item Env:AUDIOSYNC_DEBUG
  foreach ($k in $Extra.Keys) { Remove-Item "Env:$k" }
  $script:main = $null
  [void](Wait-Until { ($script:main = Get-AppWindows | Where-Object { $_.Current.Name -eq 'AudioSync' } | Select-Object -First 1) -ne $null } 30)
  if (-not $script:main) { throw 'no main window within 30 s' }
  $script:mainHandle = $script:main.Current.NativeWindowHandle
  Start-Sleep -Seconds 3
}

function Stop-App([string] $Name) {
  if (-not $script:app.HasExited) { Stop-Process -Id $script:app.Id -Force; Start-Sleep -Seconds 2 }
  if (Test-Path $log) { Copy-Item -Force $log (Join-Path $Work "audiosync-debug-$Name.log") }
}

# How each window is opened from the main window, by the key the Escape and
# F1 rows already proved or by its gear menu item.
$windows = [ordered]@{
  'main' = @{ Title = 'AudioSync' }
  'Settings' = @{ Title = 'Settings'; Open = { Press ',' -Ctrl } }
  'Equalizer' = @{ Title = 'Equalizer'; Open = { Press 'e' -Ctrl } }
  'Guide' = @{ Title = 'AudioSync Guide'; Open = { Press 'F1' } }
  'About' = @{ Title = 'About AudioSync'; Open = { (Invoke-Named 'Options') -and (Invoke-Named 'About AudioSync') | Out-Null } }
  'Web Remote' = @{ Title = 'Web Remote'; Open = { (Invoke-Named 'Options') -and (Invoke-Named "Web Remote$([char]0x2026)") | Out-Null } }
}

function Open-Under-Test([string] $Name) {
  if ($Name -eq 'main') { $script:w = $script:main; Focus-Window $script:w; return $true }
  Focus-Window $script:main
  & $windows[$Name].Open
  $title = $windows[$Name].Title
  if (-not (Wait-Until { ($script:w = Find-Window $title) -ne $null } 8)) { return $false }
  Focus-Window $script:w
  $true
}

function Close-Other([string] $Title) {
  $o = Find-Window $Title
  if ($o) { try { $o.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close() } catch { } }
  [void](Wait-Until { -not (Find-Window $Title) } 4)
}

foreach ($name in $windows.Keys) {
  $title = $windows[$name].Title
  # A leg that throws is one FAIL line; the next window still runs.
  try {
  Start-App
  if (-not (Open-Under-Test $name)) {
    Record $name 'open' $false "no '$title' window"
    Stop-App $name; continue
  }
  Save-Shot $script:w "window-$name"
  # The window is found again before each press: a rebuilt window has a new handle.
  $self = {
    if ($name -ne 'main') { $found = Find-Window $title; if ($found) { $script:w = $found } }
    Focus-Window $script:w
  }

  foreach ($pair in @(@(',', 'Settings'), @('e', 'Equalizer'))) {
    $key, $target = $pair
    & $self
    Press $key -Ctrl
    $ok = Wait-Until { (Find-Window $target) -ne $null -or ($name -eq $target -and -not $script:app.HasExited) }
    Record $name "Ctrl+$key" $ok "the $target window is shown"
    if ($name -ne $target) { Close-Other $target }
  }

  # Ctrl+N with no speaker on: the zone dialog says why, and Escape closes it.
  & $self
  Press 'n' -Ctrl
  $ok = Wait-Until { ($script:zone = Find-Window 'AudioSync') -ne $null }
  Record $name 'Ctrl+N' $ok 'the zone dialog is shown'
  if ($ok) {
    Save-Shot $script:zone "zone-dialog-from-$name"
    Focus-Window $script:zone
    Press 'Escape'
    Record $name 'Escape (zone dialog)' (Wait-Until { -not (Find-Window 'AudioSync') }) 'the zone dialog is closed'
  }

  foreach ($pair in @(@('r', $true), @('F5', $false))) {
    $key, $withCtrl = $pair
    & $self
    $before = Log-Count $refreshLine
    if ($withCtrl) { Press $key -Ctrl } else { Press $key }
    $label = if ($withCtrl) { "Ctrl+$key" } else { $key }
    Record $name $label (Wait-Until { (Log-Count $refreshLine) -gt $before }) 'the devices are listed again (debug log)'
  }

  foreach ($pair in @(@('1', 'One'), @('2', 'Two'))) {
    $digit, $preset = $pair
    & $self
    $line = "preset `"Keys $preset`""
    $before = Log-Count $line
    Press $digit -Ctrl
    Record $name "Ctrl+$digit" (Wait-Until { (Log-Count $line) -gt $before }) "preset Keys $preset is recalled (debug log)"
  }

  if ($name -ne 'main') {
    $script:main.GetCurrentPattern([System.Windows.Automation.WindowPattern]::Pattern).Close()
    [void](Wait-Until { -not (Main-Shown) } 4)
    & $self
    Press '0' -Ctrl
    Record $name 'Ctrl+0' (Wait-Until { Main-Shown }) 'the main window is shown'
  }

  & $self
  Press 'w' -Ctrl
  $gone = if ($name -eq 'main') { { -not (Main-Shown) } } else { { -not (Find-Window $title) } }
  Record $name 'Ctrl+W' ((Wait-Until $gone) -and -not $script:app.HasExited) 'the window is closed and the app still runs'

  # Ctrl+Q from the same window, opened again (the main window in a new launch).
  if ($name -eq 'main') { Stop-App $name; Start-App }
  if (Open-Under-Test $name) {
    Press 'q' -Ctrl
    Record $name 'Ctrl+Q' (Wait-Until { $script:app.HasExited } 10) 'the app quits'
  } else {
    Record $name 'Ctrl+Q' $false "no '$title' window to press it in"
  }
  Stop-App $name
  } catch {
    Record $name 'leg' $false "stopped: $($_.Exception.Message)"
    if ($script:app) { Stop-App $name }
  }
}

# The PIN dialog of a pairing, opened at launch for a made-up Apple TV.
try {
Start-App @{ AUDIOSYNC_PREVIEW_PIN = 'code' }
$pinTitle = 'Pair with Living Room'
if (Wait-Until { ($script:pin = Find-Window $pinTitle) -ne $null } 10) {
  Save-Shot $script:pin 'pin-dialog'
  Focus-Window $script:pin
  Press 'Escape'
  Record 'PIN dialog' 'Escape' (Wait-Until { -not (Find-Window $pinTitle) }) 'the PIN dialog is closed'
} else {
  Record 'PIN dialog' 'Escape' $false "no '$pinTitle' window"
}
Stop-App 'pin'
} catch { Record 'PIN dialog' 'leg' $false "stopped: $($_.Exception.Message)"; if ($script:app) { Stop-App 'pin' } }

# The Calibrate sheet, over the main window: a group named as its button.
try {
Start-App
Focus-Window $script:main
$calibrate = 'Calibrate All Speakers'
$group = New-Object System.Windows.Automation.AndCondition(
  (New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $calibrate)),
  (New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, [System.Windows.Automation.ControlType]::Group)))
$sheetShown = { $null -ne $script:main.FindFirst($Scope::Descendants, $group) }
if ((Invoke-Named $calibrate) -and (Wait-Until $sheetShown)) {
  Save-Shot $script:main 'calibrate-sheet'
  Focus-Window $script:main
  Press 'Escape'
  Record 'Calibrate sheet' 'Escape' (Wait-Until { -not (& $sheetShown) }) 'the sheet is closed'
} else {
  Record 'Calibrate sheet' 'Escape' $false 'the sheet did not open'
}
Save-Shot $script:main 'calibrate-after-escape'
Stop-App 'calibrate'
} catch { Record 'Calibrate sheet' 'leg' $false "stopped: $($_.Exception.Message)"; if ($script:app) { Stop-App 'calibrate' } }

$script:results.Add('NOT SET UP | main | Ctrl+S | applies a typed delay; the runner has no speaker row to type one in')
$script:results | Set-Content (Join-Path $Work 'results.txt')
"$($script:results.Count) lines, $script:failed failed"
if ($script:failed) { exit 1 }
exit 0
