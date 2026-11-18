# A stranger's Windows PC streaming for five minutes, run by
# .github/workflows/stranger.yml: two Scream virtual outputs (MS-PL, downloaded
# and checked by hash, never shipped) as the PC's speakers, a silent install of
# AudioSync-Setup.exe, a launch from Program Files, a tone playing to the
# default while AudioSync streams it, the private bytes sampled every 10 s,
# then the uninstall while the app runs, and the default output and every
# output's volume and mute compared to the ones found before the install.
#
# Memory bound: after the first minute (buffers, fonts and the GPU settle),
# private bytes may rise at most 1 MB a minute (least-squares slope over
# minutes 1 to 5). A per-callback leak of one 10 ms buffer is about 20 MB a
# minute; slower leaks are left to the 60-minute soak.
#
#   pwsh scripts/stranger/windows_memory.ps1 -Setup <AudioSync-Setup.exe> -Work <folder>
param(
  [Parameter(Mandatory)] [string] $Setup,
  [Parameter(Mandatory)] [string] $Work,
  [int] $Seconds = 300
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
class MMDeviceEnumeratorClass {}

[ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator {
  [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IMMDeviceCollection devices);
  [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
}

[ComImport, Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceCollection {
  [PreserveSig] int GetCount(out uint count);
  [PreserveSig] int Item(uint index, out IMMDevice device);
}

[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
  [PreserveSig] int Activate(ref Guid iid, int context, IntPtr parameters, [MarshalAs(UnmanagedType.IUnknown)] out object face);
  [PreserveSig] int OpenPropertyStore(int access, out IntPtr store);
  [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
}

// endpointvolume.h's order, up to GetMute.
[ComImport, Guid("5CDF2C82-841E-4546-9722-0CF74078229A"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IAudioEndpointVolume {
  [PreserveSig] int RegisterControlChangeNotify(IntPtr notify);
  [PreserveSig] int UnregisterControlChangeNotify(IntPtr notify);
  [PreserveSig] int GetChannelCount(out uint count);
  [PreserveSig] int SetMasterVolumeLevel(float db, ref Guid context);
  [PreserveSig] int SetMasterVolumeLevelScalar(float level, ref Guid context);
  [PreserveSig] int GetMasterVolumeLevel(out float db);
  [PreserveSig] int GetMasterVolumeLevelScalar(out float level);
  [PreserveSig] int SetChannelVolumeLevel(uint channel, float db, ref Guid context);
  [PreserveSig] int SetChannelVolumeLevelScalar(uint channel, float level, ref Guid context);
  [PreserveSig] int GetChannelVolumeLevel(uint channel, out float db);
  [PreserveSig] int GetChannelVolumeLevelScalar(uint channel, out float level);
  [PreserveSig] int SetMute([MarshalAs(UnmanagedType.Bool)] bool mute, ref Guid context);
  [PreserveSig] int GetMute([MarshalAs(UnmanagedType.Bool)] out bool mute);
}

public static class Endpoints {
  const int Render = 0, Active = 1;
  static Guid Writer = Guid.NewGuid();

  static IMMDeviceEnumerator Enumerator() {
    return (IMMDeviceEnumerator)new MMDeviceEnumeratorClass();
  }

  // role 0 is eConsole, 1 eMultimedia.
  public static string Default(int role) {
    IMMDevice device;
    int hr = Enumerator().GetDefaultAudioEndpoint(Render, role, out device);
    if (hr != 0) return string.Format("none (0x{0:X8})", hr);
    string id;
    hr = device.GetId(out id);
    return hr == 0 ? id : string.Format("unreadable (0x{0:X8})", hr);
  }

  public static int ActiveCount() {
    try {
      IMMDeviceCollection devices;
      if (Enumerator().EnumAudioEndpoints(Render, Active, out devices) != 0) return 0;
      uint count;
      return devices.GetCount(out count) == 0 ? (int)count : 0;
    } catch (Exception) {
      return 0;
    }
  }

  static IAudioEndpointVolume Volume(IMMDevice device) {
    Guid iid = typeof(IAudioEndpointVolume).GUID;
    object face;
    Marshal.ThrowExceptionForHR(device.Activate(ref iid, 23, IntPtr.Zero, out face));
    return (IAudioEndpointVolume)face;
  }

  // The same level and unmuted on every active output, so "as found" is known.
  public static void SetAll(float level) {
    IMMDeviceCollection devices;
    Marshal.ThrowExceptionForHR(Enumerator().EnumAudioEndpoints(Render, Active, out devices));
    uint count;
    Marshal.ThrowExceptionForHR(devices.GetCount(out count));
    for (uint i = 0; i < count; i++) {
      IMMDevice device;
      Marshal.ThrowExceptionForHR(devices.Item(i, out device));
      var volume = Volume(device);
      Marshal.ThrowExceptionForHR(volume.SetMute(false, ref Writer));
      Marshal.ThrowExceptionForHR(volume.SetMasterVolumeLevelScalar(level, ref Writer));
    }
  }

  // Every active output as "<id> <level>% muted|unmuted", one per line.
  public static string Readings() {
    var lines = new System.Text.StringBuilder();
    IMMDeviceCollection devices;
    Marshal.ThrowExceptionForHR(Enumerator().EnumAudioEndpoints(Render, Active, out devices));
    uint count;
    Marshal.ThrowExceptionForHR(devices.GetCount(out count));
    for (uint i = 0; i < count; i++) {
      IMMDevice device;
      Marshal.ThrowExceptionForHR(devices.Item(i, out device));
      string id;
      Marshal.ThrowExceptionForHR(device.GetId(out id));
      var volume = Volume(device);
      float level;
      bool muted;
      Marshal.ThrowExceptionForHR(volume.GetMasterVolumeLevelScalar(out level));
      Marshal.ThrowExceptionForHR(volume.GetMute(out muted));
      lines.AppendFormat("{0} {1:F1}% {2}\n", id, level * 100, muted ? "muted" : "unmuted");
    }
    return lines.ToString().TrimEnd();
  }
}
'@

$script:failed = @()
function Check([bool] $Ok, [string] $What) {
  $line = "$(if ($Ok) { 'PASS' } else { 'FAIL' }) A7.3: $What"
  $line | Tee-Object -FilePath "$Work/result.txt" -Append
  if (-not $Ok) { $script:failed += $line }
}
function Get-Found { "console=$([Endpoints]::Default(0)) multimedia=$([Endpoints]::Default(1))`n$([Endpoints]::Readings())" }

$os = Get-CimInstance Win32_OperatingSystem
"os: $($os.Caption) build $($os.BuildNumber)" | Tee-Object -FilePath "$Work/env.txt"
foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') {
  Set-Service -Name $name -StartupType Automatic
  Start-Service -Name $name
}

# Scream 4.0 as windows_default_device.ps1 installs it: the catalog signed again
# with a certificate this machine trusts; two ROOT devices, two outputs.
$fixture = (New-Item -ItemType Directory -Force -Path (Join-Path ([IO.Path]::GetTempPath()) 'audiosync-ci-memory')).FullName
$zip = Join-Path $fixture 'Scream4.0.zip'
Invoke-WebRequest -Uri 'https://github.com/duncanthrax/scream/releases/download/4.0/Scream4.0.zip' -OutFile $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash
if ($hash -ne 'FA33E25F9A46C61E4E0CD83362C51C3D2A45C6FE4091AAD7507E240E40F1A520') { throw "Scream4.0.zip hash $hash" }
Expand-Archive -Force $zip (Join-Path $fixture 'scream')
$driver = Join-Path $fixture 'scream/Install/driver/x64'
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AudioSync CI test driver' -CertStoreLocation Cert:\LocalMachine\My
$cer = Join-Path $fixture 'ci-driver.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
& $signtool.FullName sign /fd SHA256 /sm /s My /sha1 $cert.Thumbprint (Join-Path $driver 'scream.cat')
if ($LASTEXITCODE -ne 0) { throw "signtool could not sign the catalog (exit $LASTEXITCODE)" }
pnputil /add-driver (Join-Path $driver 'Scream.inf') /install | Tee-Object -FilePath "$Work/env.txt" -Append
$devcon = Join-Path $fixture 'scream/Install/helpers/devcon-x64.exe'
$count = 0
for ($n = 1; $n -le 2; $n++) {
  & $devcon install (Join-Path $driver 'Scream.inf') '*Scream' | Tee-Object -FilePath "$Work/env.txt" -Append
  for ($i = 0; $i -lt 60 -and $count -lt $n; $i++) { Start-Sleep -Seconds 1; $count = [Endpoints]::ActiveCount() }
}
"active output endpoints: $count" | Tee-Object -FilePath "$Work/env.txt" -Append
if ($count -ne 2) { throw "$count active output endpoints after adding two (two needed)" }

[Endpoints]::SetAll(0.42)
$found = Get-Found
"before the install:`n$found" | Tee-Object -FilePath "$Work/result.txt" -Append

$install = Start-Process -FilePath $Setup -Wait -PassThru `
  -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$Work\install.log`""
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
if ($install.ExitCode -ne 0 -or -not (Test-Path $exe)) { throw "install failed (exit $($install.ExitCode))" }
"installed: $exe $((Get-Item $exe).VersionInfo.ProductVersion)" | Tee-Object -FilePath "$Work/result.txt" -Append

# A 440 Hz tone looping on the default, so the stream carries sound.
$wav = Join-Path $fixture 'tone.wav'
$samples = 44100
$stream = [IO.File]::Create($wav)
$w = New-Object IO.BinaryWriter $stream
$w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + 2 * $samples)); $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
$w.Write([int]16); $w.Write([int16]1); $w.Write([int16]1); $w.Write([int]44100); $w.Write([int](2 * 44100)); $w.Write([int16]2); $w.Write([int16]16)
$w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int](2 * $samples))
for ($i = 0; $i -lt $samples; $i++) { $w.Write([int16](8000 * [math]::Sin(2 * [math]::PI * 440 * $i / 44100))) }
$w.Close()
$player = Start-Process -FilePath powershell.exe -WindowStyle Hidden -PassThru `
  -ArgumentList '-NoProfile', '-Command', "(New-Object Media.SoundPlayer '$wav').PlayLooping(); Start-Sleep -Seconds 900"

# Show Virtual Devices on: ROOT-enumerated outputs are listed as virtual.
$data = Join-Path $env:LOCALAPPDATA 'AudioSync'
New-Item -ItemType Directory -Force -Path $data | Out-Null
'{"version":3,"show_virtual_devices":true}' | Set-Content -Encoding ascii (Join-Path $data 'settings.json')
$log = Join-Path $env:TEMP 'audiosync-debug.log'
$env:AUDIOSYNC_DEBUG = '1'
$app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
Remove-Item Env:AUDIOSYNC_DEBUG

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$TogglePattern = [System.Windows.Automation.TogglePattern]::Pattern
function Get-Toggles {
  $pid_ = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $app.Id)
  $c = New-Object System.Windows.Automation.PropertyCondition($A::IsTogglePatternAvailableProperty, $true)
  @(foreach ($w in $A::RootElement.FindAll($Scope::Children, $pid_)) { $w.FindAll($Scope::Descendants, $c) })
}
function Test-On($Element) { $Element.GetCurrentPattern($TogglePattern).Current.ToggleState -eq 'On' }

$toggles = @()
for ($i = 0; $i -lt 30 -and -not ($toggles | Where-Object { $_.Current.Name -eq 'Streaming' }); $i++) { Start-Sleep -Seconds 1; $toggles = Get-Toggles }
Start-Sleep -Seconds 3
$toggles = Get-Toggles
"toggles: $(($toggles | ForEach-Object { "'$($_.Current.Name)'=$(Test-On $_)" }) -join ', ')" | Tee-Object -FilePath "$Work/result.txt" -Append
$streaming = $toggles | Where-Object { $_.Current.Name -eq 'Streaming' } | Select-Object -First 1
$rows = @($toggles | Where-Object { $_.Current.Name -match 'Speakers|Scream' })
if (-not $streaming -or $rows.Count -lt 2) { throw 'no Streaming switch or fewer than two Scream rows (see toggles above)' }
foreach ($row in $rows) { if (-not (Test-On $row)) { $row.GetCurrentPattern($TogglePattern).Toggle(); Start-Sleep -Seconds 2 } }
if (-not (Test-On $streaming)) { $streaming.GetCurrentPattern($TogglePattern).Toggle() }
Start-Sleep -Seconds 6
Check ((-not $app.HasExited) -and (Test-On $streaming)) "streaming to both outputs (default while streaming: $([Endpoints]::Default(0)))"

$csv = Join-Path $Work 'memory.csv'
'seconds,private_bytes' | Set-Content -Encoding ascii $csv
$points = @()
for ($t = 0; $t -le $Seconds -and -not $app.HasExited; $t += 10) {
  $app.Refresh()
  $points += [pscustomobject]@{ T = $t; B = $app.PrivateMemorySize64 }
  "$t,$($app.PrivateMemorySize64)" | Add-Content -Encoding ascii $csv
  if ($t -lt $Seconds) { Start-Sleep -Seconds 10 }
}
Check ((-not $app.HasExited) -and (Test-On $streaming)) "the app runs and streams after $Seconds s"
$settled = @($points | Where-Object { $_.T -ge 60 })
$mx = ($settled | Measure-Object T -Average).Average
$my = ($settled | Measure-Object B -Average).Average
$sxy = 0.0; $sxx = 0.0
foreach ($p in $settled) { $sxy += ($p.T - $mx) * ($p.B - $my); $sxx += ($p.T - $mx) * ($p.T - $mx) }
$slope = if ($sxx -gt 0) { $sxy / $sxx * 60 / 1MB } else { [double]::NaN }
$first = [math]::Round($points[0].B / 1MB, 1)
$last = [math]::Round($points[-1].B / 1MB, 1)
Check ($settled.Count -ge 20 -and $slope -lt 1.0) ("private bytes {0} MB at the start, {1} MB at {2} s; slope over minutes 1-5 {3:F2} MB/min (bound 1)" -f $first, $last, $points[-1].T, $slope)

Stop-Process -Id $player.Id -Force -ErrorAction SilentlyContinue
if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }

# The uninstaller closes the running app itself.
Start-Process -FilePath (Join-Path $dir 'unins000.exe') -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
for ($i = 0; $i -lt 60 -and (Test-Path $exe); $i++) { Start-Sleep -Seconds 1 }
Check (-not (Test-Path $exe) -and $app.HasExited) 'uninstalled, and the app no longer runs'
if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force }
for ($i = 0; $i -lt 15 -and (Get-Found) -ne $found; $i++) { Start-Sleep -Seconds 1 }
$now = Get-Found
"after the uninstall:`n$now" | Tee-Object -FilePath "$Work/result.txt" -Append
Check ($now -eq $found) "after the uninstall the default output and every output's volume and mute are as found"

if ($script:failed) { "$($script:failed.Count) checks failed"; exit 1 }
'every memory and uninstall check passed'
exit 0
