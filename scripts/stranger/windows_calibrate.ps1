# Calibrate All on a clean Windows runner through a digital loop, run by
# .github/workflows/calibrate-loop-windows.yml. Two speakers and a microphone:
#   Scream (MS-PL, checked by hash), whose stream scream_relay.py plays into VB-CABLE's input;
#   VB-CABLE's input itself (donationware, from the vendor, runner only);
#   VB-CABLE's output as the only microphone, so it hears both speakers.
# It walks Before you start with the reference lowered to 20% and presses its
# Set to 75%, runs Calibrate All to Results, presses Measure Again and Apply,
# then runs once more and fails unless every applied speaker is within 5 ms.
#
#   pwsh scripts/stranger/windows_calibrate.ps1 -Exe <audiosync.exe> -Work <folder>
param(
  [Parameter(Mandatory)] [string] $Exe,
  [Parameter(Mandatory)] [string] $Work
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path
$Exe = (Resolve-Path $Exe).Path
$here = $PSScriptRoot

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
class MMDeviceEnumeratorClass {}

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator {
  [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IMMDeviceCollection devices);
}

[ComImport, Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceCollection {
  [PreserveSig] int GetCount(out uint count);
  [PreserveSig] int Item(uint index, out IMMDevice device);
}

[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
  [PreserveSig] int Activate(ref Guid iid, int context, IntPtr parameters, [MarshalAs(UnmanagedType.IUnknown)] out object face);
}

// endpointvolume.h's order, up to SetMasterVolumeLevelScalar.
[ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioEndpointVolume {
  [PreserveSig] int RegisterControlChangeNotify(IntPtr notify);
  [PreserveSig] int UnregisterControlChangeNotify(IntPtr notify);
  [PreserveSig] int GetChannelCount(out uint count);
  [PreserveSig] int SetMasterVolumeLevel(float db, ref Guid context);
  [PreserveSig] int SetMasterVolumeLevelScalar(float level, ref Guid context);
}

public static class Endpoints {
  static Guid Writer = Guid.NewGuid();

  static IMMDeviceCollection Active(int flow) {
    IMMDeviceCollection devices;
    Marshal.ThrowExceptionForHR(((IMMDeviceEnumerator)new MMDeviceEnumeratorClass()).EnumAudioEndpoints(flow, 1, out devices));
    return devices;
  }

  // flow 0 is render, 1 capture.
  public static int Count(int flow) {
    try {
      uint count;
      return Active(flow).GetCount(out count) == 0 ? (int)count : 0;
    } catch (Exception) {
      return 0;
    }
  }

  public static void SetAllLevels(float level) {
    IMMDeviceCollection devices = Active(0);
    uint count;
    Marshal.ThrowExceptionForHR(devices.GetCount(out count));
    for (uint i = 0; i < count; i++) {
      IMMDevice device;
      Marshal.ThrowExceptionForHR(devices.Item(i, out device));
      Guid iid = typeof(IAudioEndpointVolume).GUID;
      object face;
      Marshal.ThrowExceptionForHR(device.Activate(ref iid, 23, IntPtr.Zero, out face));
      Marshal.ThrowExceptionForHR(((IAudioEndpointVolume)face).SetMasterVolumeLevelScalar(level, ref Writer));
    }
  }
}
'@

function Note([string] $Line) { $Line | Tee-Object -FilePath "$Work/result.txt" -Append }

foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') {
  Set-Service -Name $name -StartupType Automatic
  Start-Service -Name $name
}
# The microphone allowed for desktop apps, as a user who said yes would have it.
foreach ($root in 'HKLM:', 'HKCU:') {
  $key = "$root\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone"
  New-Item -Force -Path $key | Out-Null
  Set-ItemProperty -Path $key -Name Value -Value Allow
}

$fixture = (New-Item -ItemType Directory -Force -Path (Join-Path ([IO.Path]::GetTempPath()) 'audiosync-ci-driver')).FullName
function Get-Checked([string] $Url, [string] $Name, [string] $Sha256) {
  $zip = Join-Path $fixture $Name
  Invoke-WebRequest -Uri $Url -OutFile $zip
  $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash
  if ($hash -ne $Sha256) { throw "$Name hash $hash" }
  $into = Join-Path $fixture ([IO.Path]::GetFileNameWithoutExtension($Name))
  Expand-Archive -Force $zip $into
  $into
}

# Scream sends to the relay on 127.0.0.1 rather than to its multicast group.
$options = 'HKLM:\SYSTEM\CurrentControlSet\Services\Scream\Options'
New-Item -Force -Path $options | Out-Null
Set-ItemProperty -Path $options -Name UnicastIPv4 -Value '127.0.0.1'
Set-ItemProperty -Path $options -Name UnicastPort -Value 4010 -Type DWord

$scream = Get-Checked 'https://github.com/duncanthrax/scream/releases/download/4.0/Scream4.0.zip' 'Scream4.0.zip' `
  'FA33E25F9A46C61E4E0CD83362C51C3D2A45C6FE4091AAD7507E240E40F1A520'
$driver = Join-Path $scream 'Install/driver/x64'
$devcon = Join-Path $scream 'Install/helpers/devcon-x64.exe'
# The catalog signed again with a certificate this machine trusts, as windows_default_device.ps1 does.
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AudioSync CI test driver' -CertStoreLocation Cert:\LocalMachine\My
$cer = Join-Path $fixture 'ci-driver.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
& $signtool.FullName sign /fd SHA256 /sm /s My /sha1 $cert.Thumbprint (Join-Path $driver 'scream.cat')
if ($LASTEXITCODE -ne 0) { throw "signtool could not sign the Scream catalog (exit $LASTEXITCODE)" }
pnputil /add-driver (Join-Path $driver 'Scream.inf') /install | Out-File -Append "$Work/env.txt"
& $devcon install (Join-Path $driver 'Scream.inf') '*Scream' | Out-File -Append "$Work/env.txt"

# VB-CABLE 46 from the vendor; its publisher trusted first so devcon installs it without a prompt.
$cable = Get-Checked 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack46.zip' 'VBCABLE_Driver_Pack46.zip' `
  'BC44E8974191A5A2427D6C979E2DAB08DBB09925F3F435DFA2F0BA3E45B6EB90'
$signer = (Get-AuthenticodeSignature (Join-Path $cable 'vbaudio_cable64_win10.cat')).SignerCertificate
if ($signer) {
  $vbCer = Join-Path $fixture 'vbcable.cer'
  [IO.File]::WriteAllBytes($vbCer, $signer.Export('Cert'))
  Import-Certificate -FilePath $vbCer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
}
pnputil /add-driver (Join-Path $cable 'vbMmeCable64_win10.inf') /install | Out-File -Append "$Work/env.txt"
& $devcon install (Join-Path $cable 'vbMmeCable64_win10.inf') 'VBAudioVACWDM' | Out-File -Append "$Work/env.txt"

for ($i = 0; $i -lt 60 -and ([Endpoints]::Count(0) -lt 2 -or [Endpoints]::Count(1) -lt 1); $i++) { Start-Sleep -Seconds 1 }
Get-PnpDevice -PresentOnly -Class MEDIA, AudioEndpoint -ErrorAction SilentlyContinue |
  Format-Table -AutoSize Status, Class, FriendlyName, InstanceId | Out-String -Width 200 | Out-File -Append "$Work/env.txt"
Note "outputs: $([Endpoints]::Count(0)), inputs: $([Endpoints]::Count(1))"
if ([Endpoints]::Count(0) -lt 2 -or [Endpoints]::Count(1) -lt 1) { throw 'the drivers gave fewer than two outputs and one input (see env.txt)' }

$relay = Start-Process -FilePath python -ArgumentList '-I', (Join-Path $here 'scream_relay.py'), '4010', 'Speakers (VB-Audio' -PassThru `
  -RedirectStandardOutput "$Work/relay.out" -RedirectStandardError "$Work/relay.err"

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$Invoke = [System.Windows.Automation.InvokePattern]::Pattern
$Toggle = [System.Windows.Automation.TogglePattern]::Pattern

function Get-AppWindows {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $script:app.Id)
  @($A::RootElement.FindAll($Scope::Children, $c))
}

# Every element of the app whose name passes $Test and that has an Invoke or Toggle pattern.
function Find-Actionable([scriptblock] $Test, [int] $Seconds = 10) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  do {
    $found = @(foreach ($w in Get-AppWindows) {
        foreach ($e in $w.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) {
          $p = $null
          if ((& $Test $e.Current.Name) -and ($e.TryGetCurrentPattern($Invoke, [ref]$p) -or $e.TryGetCurrentPattern($Toggle, [ref]$p))) { $e }
        }
      })
    if ($found) { return $found }
    Start-Sleep -Milliseconds 500
  } while ((Get-Date) -lt $deadline)
  @()
}

function Use([System.Windows.Automation.AutomationElement] $Element) {
  $p = $null
  if ($Element.TryGetCurrentPattern($Invoke, [ref]$p)) { $p.Invoke() } else { $Element.GetCurrentPattern($Toggle).Toggle() }
  Start-Sleep -Seconds 2
}

function Use-Named([string] $Name, [int] $Seconds = 10) {
  $e = @(Find-Actionable { param($n) $n -eq $Name } $Seconds) | Select-Object -First 1
  if (-not $e) { Note "not reached: no '$Name'"; return $false }
  Use $e
  $true
}

function Test-Named([string] $Name) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $Name)
  [bool]@(foreach ($w in Get-AppWindows) { $w.FindAll($Scope::Descendants, $c) })
}

function Save-Shot([string] $State) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $shot = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen($b.X, $b.Y, 0, 0, $shot.Size)
  $shot.Save((Join-Path $Work "$State.png"), [System.Drawing.Imaging.ImageFormat]::Png)
  Note "shot: $State.png"
}

function Save-Tree([string] $State) {
  @(foreach ($w in Get-AppWindows) {
      foreach ($e in $w.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) {
        if ($e.Current.Name) { "$($e.Current.ControlType.ProgrammaticName) '$($e.Current.Name)'" }
      }
    }) | Set-Content (Join-Path $Work "uia-$State.txt")
}

$log = Join-Path $env:TEMP 'audiosync-debug.log'
function Get-Log {
  try {
    $stream = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite')
    try { (New-Object IO.StreamReader($stream)).ReadToEnd() } finally { $stream.Dispose() }
  } catch { '' }
}
function Wait-Log([string] $Pattern, [int] $After, [int] $Seconds) {
  $deadline = (Get-Date).AddSeconds($Seconds)
  do {
    $m = [regex]::Match((Get-Log).Substring([math]::Min($After, (Get-Log).Length)), $Pattern)
    if ($m.Success) { return $m.Value }
    Start-Sleep -Seconds 1
  } while ((Get-Date) -lt $deadline)
  $null
}

$data = Join-Path $env:LOCALAPPDATA 'AudioSync'
New-Item -ItemType Directory -Force -Path $data | Out-Null
'{"version":3,"appearance":"light","show_virtual_devices":true}' | Set-Content -Encoding ascii (Join-Path $data 'settings.json')
$env:AUDIOSYNC_DEBUG = '1'
$script:app = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path $Exe) -PassThru
Remove-Item Env:AUDIOSYNC_DEBUG
for ($i = 0; $i -lt 30 -and -not @(Get-AppWindows); $i++) { Start-Sleep -Seconds 1 }
Start-Sleep -Seconds 3
if (-not @(Get-AppWindows)) { throw 'no AudioSync window within 30 s' }
Save-Tree 'ready'

foreach ($row in Find-Actionable { param($n) $n -match 'Scream|Speakers|CABLE' -and $n -notmatch 'Calibrate' }) {
  $p = $null
  if ($row.TryGetCurrentPattern($Toggle, [ref]$p) -and $p.Current.ToggleState -ne 'On') { Use $row }
}
Note "rows: $((Find-Actionable { param($n) $n -match 'Scream|Speakers|CABLE|Streaming' } 2 | ForEach-Object { "'$($_.Current.Name)'" }) -join ', ')"
$streaming = @(Find-Actionable { param($n) $n -eq 'Streaming' }) | Select-Object -First 1
if ($streaming -and $streaming.GetCurrentPattern($Toggle).Current.ToggleState -ne 'On') { Use $streaming }
Start-Sleep -Seconds 6
Save-Shot 'CAL-READY'

# Before you start with the reference low, and its Set to 75%.
[Endpoints]::SetAllLevels(0.2)
Start-Sleep -Seconds 3
$opener = @(Find-Actionable { param($n) $n -eq 'Calibrate All Speakers' }) | Select-Object -First 1
function Open-Sheet {
  if (-not $opener) { return $false }
  Use $opener
  for ($i = 0; $i -lt 20; $i++) { if ((Test-Named 'Before you start') -or (Test-Named 'Results')) { return $true }; Start-Sleep -Milliseconds 500 }
  $false
}
if (Open-Sheet) {
  Save-Shot 'CAL-ALL-BEFORE'
  Save-Tree 'all-before'
  $at = (Get-Log).Length
  $fix = @(Find-Actionable { param($n) $n -like 'Set to 75%, *' } 5) | Select-Object -First 1
  if ($fix) {
    Use $fix
    Note "set to 75%: $(Wait-Log 'device volume fix: volume set to 75%: [^\r\n]*' $at 15)"
    Save-Shot 'CAL-ALL-VOLUME-FIXED'
  } else { Note 'not reached: no Set to 75% on Before you start' }
  # Back to full scale for the speaker that isn't the reference.
  [Endpoints]::SetAllLevels(1.0)
} else { Note 'not reached: no Calibrate All sheet' }

# A run to its end; one the virtual microphone dropped audio in runs again, up to twice.
function Start-Run([string] $State) {
  for ($attempt = 0; $attempt -lt 3; $attempt++) {
    if (-not ((Test-Named 'Before you start') -or (Open-Sheet))) { return $null }
    $start = @(Find-Actionable { param($n) $n -eq 'Calibrate All Speakers' }) |
      Where-Object { -not [System.Windows.Automation.Automation]::Compare($_, $opener) } | Select-Object -First 1
    if (-not $start) { Note "not reached: $State has no start button"; return $null }
    $at = (Get-Log).Length
    Use $start
    $line = Wait-Log 'Calibrate All ended[^\r\n]*' $at 240
    if ($attempt -lt 2 -and (Get-Log).Substring($at).Contains('stopped: MicrophoneSkipped')) {
      Note "${State}: the microphone skipped, running once more"
      Use-Named 'Close the calibration results' 3 | Out-Null
      continue
    }
    return $line
  }
}

if (Start-Run 'CAL-ALL-RESULTS') {
  Start-Sleep -Seconds 2
  Save-Shot 'CAL-ALL-RESULTS'
  Save-Tree 'all-results'
  $again = @(Find-Actionable { param($n) $n -like 'Measure Again, *' } 5) | Select-Object -First 1
  if ($again) {
    $at = (Get-Log).Length
    Use $again
    Note "measure again: $(Wait-Log 'Measure Again of \S+ started' $at 10)"
    Wait-Log 'Calibrate All ended' $at 120 | Out-Null
    Start-Sleep -Seconds 2
    Save-Shot 'CAL-ALL-MEASURED-AGAIN'
  } else { Note 'not reached: no Measure Again in Results' }
  $at = (Get-Log).Length
  if (Use-Named 'Apply' 5) {
    Note "apply: $(Wait-Log 'Calibrate All (?:applied|: Apply refused)[^\r\n]*' $at 10)"
    Save-Shot 'CAL-ALL-APPLIED'
  }
  Use-Named 'Done with the calibration results' 3 | Out-Null
}
Start-Sleep -Seconds 3
if (Start-Run 'CAL-ALL-AGAIN') {
  Start-Sleep -Seconds 2
  Save-Shot 'CAL-ALL-AGAIN'
  Save-Tree 'all-again'
}

if (-not $script:app.HasExited) { Stop-Process -Id $script:app.Id -Force }
if (-not $relay.HasExited) { Stop-Process -Id $relay.Id -Force }
Start-Sleep -Seconds 2
Copy-Item $log (Join-Path $Work 'audiosync-debug.log') -ErrorAction SilentlyContinue
Copy-Item (Join-Path $data 'settings.json') (Join-Path $Work 'settings-after.json') -ErrorAction SilentlyContinue

# The record: Set to 75%, Measure Again, Apply, and every applied speaker within 5 ms in the run after.
$text = Get-Content -Raw (Join-Path $Work 'audiosync-debug.log') -ErrorAction SilentlyContinue
if (-not $text) { $text = '' }
$failed = @()
foreach ($line in [regex]::Matches($text, '[^\r\n]*(?:device volume fix|device volume: low)[^\r\n]*')) { Note "volume: $($line.Value)" }
if ($text -notmatch 'device volume fix: volume set to 75%: done') { $failed += 'Set to 75% was not written to the low reference' }
if ($text -notmatch 'Measure Again of \S+ started(?:(?!Calibrate All started)[\s\S])*?Calibrate All ended') { $failed += 'Measure Again did not start and run to its end' }
$applied = @([regex]::Matches($text, 'Calibrate All applied[^\r\n]*') | ForEach-Object Value)
$applied | ForEach-Object { Note "applied: $_" }
$ended = @([regex]::Matches($text, 'Calibrate All ended[^\r\n]*') | ForEach-Object Value)
if (-not $applied) { $failed += 'Calibrate All was never applied' }
if ($ended.Count -lt 2) { $failed += 'Calibrate All did not run to its end twice' }
else {
  $last = $ended[-1]
  if (-not $last.StartsWith('Calibrate All ended (None)')) { $failed += 'the run after Apply did not run to its end' }
  foreach ($m in [regex]::Matches($last, 'id: "([^"]+)"[^}]*?difference_ms: (-?[\d.]+)')) {
    $off = [double]$m.Groups[2].Value
    Note "after Apply: $($m.Groups[1].Value) still $([math]::Round($off, 1)) ms off"
    if ([math]::Abs($off) -gt 5) { $failed += "$($m.Groups[1].Value) is $([math]::Round($off, 1)) ms off after Apply" }
  }
  foreach ($line in $applied) {
    foreach ($m in [regex]::Matches($line, 'target: Speaker\("([^"]+)"\)')) {
      if ($last -notmatch ('id: "' + [regex]::Escape($m.Groups[1].Value) + '"[^}]*?difference_ms')) {
        $failed += "$($m.Groups[1].Value) was applied but not measured in the run after Apply"
      }
    }
  }
}
if ($text -match 'panic') { $failed += 'the debug log has a panic' }
$failed | ForEach-Object { Note "FAIL: $_" }
if ($failed) { exit 1 }
Note 'PASS'
exit 0
