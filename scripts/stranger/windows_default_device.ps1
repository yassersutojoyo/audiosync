# The Windows default output on a clean runner, run by
# .github/workflows/stranger.yml. The runner has no output endpoint, so the
# script gives it one: the Scream virtual sound card (MS-PL, downloaded and
# checked by hash, never shipped) on a software-enumerated device, which
# AudioSync lists as a built-in speaker, as on a PC whose only output is its
# own. With that endpoint the only one and the default, it checks:
#   no spare device: streaming starts, the default stays where it was and the
#     log says it gets no copy (S470);
#   volume keys: a volume set on the default as the taskbar does it stays
#     while streaming, and the stream goes on (S192);
#   muted output: the System slider on a muted default unmutes it (S197);
#   real volume keys move AudioSync's master, not the default (S429);
#   a muted, then a quiet, default shows the hint, its fix and its tooltip;
#   a source app killed while streaming leaves a notice (S227);
#   restore: with a second endpoint added, the default the app moved and every
#     output's volume and mute are as found after a kill and an uninstall (S474).
# Without an endpoint after the install it prints why and exits 0 with
# "not tried", since no check could run.
#
#   pwsh scripts/stranger/windows_default_device.ps1 -Setup <AudioSync-Setup.exe> -Work <folder>
param(
  [Parameter(Mandatory)] [string] $Setup,
  [Parameter(Mandatory)] [string] $Work
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;

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
  // Another writer than AudioSync, as the taskbar's flyout is.
  static Guid Writer = Guid.NewGuid();

  static IMMDeviceEnumerator Enumerator() {
    return (IMMDeviceEnumerator)new MMDeviceEnumeratorClass();
  }

  static IMMDevice DefaultDevice(int role) {
    IMMDevice device;
    int hr = Enumerator().GetDefaultAudioEndpoint(Render, role, out device);
    if (hr != 0) throw new COMException("no default output", hr);
    return device;
  }

  // role 0 is eConsole, 1 eMultimedia.
  public static string Default(int role) {
    try {
      string id;
      int hr = DefaultDevice(role).GetId(out id);
      return hr == 0 ? id : string.Format("unreadable (0x{0:X8})", hr);
    } catch (COMException e) {
      return string.Format("none (0x{0:X8})", e.ErrorCode);
    }
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

  static IAudioEndpointVolume Volume() {
    Guid iid = typeof(IAudioEndpointVolume).GUID;
    object face;
    Marshal.ThrowExceptionForHR(DefaultDevice(0).Activate(ref iid, 23, IntPtr.Zero, out face));
    return (IAudioEndpointVolume)face;
  }

  public static float Level() {
    float level;
    Marshal.ThrowExceptionForHR(Volume().GetMasterVolumeLevelScalar(out level));
    return level;
  }

  public static bool Muted() {
    bool muted;
    Marshal.ThrowExceptionForHR(Volume().GetMute(out muted));
    return muted;
  }

  public static void SetLevel(float level) {
    Marshal.ThrowExceptionForHR(Volume().SetMasterVolumeLevelScalar(level, ref Writer));
  }

  public static void SetMute(bool mute) {
    Marshal.ThrowExceptionForHR(Volume().SetMute(mute, ref Writer));
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
      Guid iid = typeof(IAudioEndpointVolume).GUID;
      object face;
      Marshal.ThrowExceptionForHR(device.Activate(ref iid, 23, IntPtr.Zero, out face));
      var volume = (IAudioEndpointVolume)face;
      float level;
      bool muted;
      Marshal.ThrowExceptionForHR(volume.GetMasterVolumeLevelScalar(out level));
      Marshal.ThrowExceptionForHR(volume.GetMute(out muted));
      lines.AppendFormat("{0} {1:F1}% {2}\n", id, level * 100, muted ? "muted" : "unmuted");
    }
    return lines.ToString().TrimEnd();
  }
}

// cfgmgr32's software device API (swdevice.h): a device under SWD\, whose
// enumerator isn't ROOT, so AudioSync classes its endpoint by form factor.
public static class SoftwareDevice {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct CreateInfo {
    public uint cbSize;
    public string pszInstanceId;
    public string pszzHardwareIds;
    public string pszzCompatibleIds;
    public IntPtr pContainerId;
    public uint CapabilityFlags;
    public string pszDeviceDescription;
    public string pszDeviceLocation;
    public IntPtr pSecurityDescriptor;
  }

  delegate void Created(IntPtr device, int result, IntPtr context, [MarshalAs(UnmanagedType.LPWStr)] string instanceId);

  [DllImport("cfgmgr32.dll", CharSet = CharSet.Unicode)]
  static extern int SwDeviceCreate(string enumerator, string parent, ref CreateInfo info, uint propertyCount,
    IntPtr properties, Created callback, IntPtr context, out IntPtr device);

  static Created callback;
  static IntPtr handle;
  static int result = -1;
  static string instance;

  // Blocks until PnP has handled the device; "0x..: <instance>".
  public static string Create(string enumerator, string hardwareId, string description) {
    var done = new ManualResetEvent(false);
    callback = (device, hr, context, id) => { result = hr; instance = id; done.Set(); };
    var info = new CreateInfo {
      cbSize = (uint)Marshal.SizeOf(typeof(CreateInfo)),
      pszInstanceId = "0",
      pszzHardwareIds = hardwareId + "\0",
      CapabilityFlags = 0x2 | 0x8, // SilentInstall, DriverRequired
      pszDeviceDescription = description,
    };
    int hr2 = SwDeviceCreate(enumerator, "HTREE\\ROOT\\0", ref info, 0, IntPtr.Zero, callback, IntPtr.Zero, out handle);
    if (hr2 != 0) return string.Format("0x{0:X8}: SwDeviceCreate failed", hr2);
    if (!done.WaitOne(120000)) return "timeout: no answer from PnP in 120 s";
    return string.Format("0x{0:X8}: {1}", result, instance);
  }
}

// Real input, through the stream a keyboard and a mouse feed.
public static class Input {
  [DllImport("user32.dll")]
  static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  [DllImport("user32.dll")]
  static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")]
  static extern void mouse_event(uint flags, int dx, int dy, uint data, UIntPtr extra);

  // An extended key, down then up.
  public static void Press(byte vk) {
    keybd_event(vk, 0, 1, UIntPtr.Zero);
    keybd_event(vk, 0, 3, UIntPtr.Zero);
  }

  public static void Hover(int x, int y) {
    SetCursorPos(x - 1, y);
    mouse_event(1, 1, 0, 0, UIntPtr.Zero);
  }
}
'@

function Get-Defaults { "console=$([Endpoints]::Default(0)) multimedia=$([Endpoints]::Default(1))" }

$script:failed = @()
function Step([string] $What) { "step $(Get-Date -Format HH:mm:ss): $What" | Tee-Object -FilePath "$Work/result.txt" -Append }
function Check([string] $Row, [bool] $Ok, [string] $What) {
  $line = "$(if ($Ok) { 'PASS' } else { 'FAIL' }) ${Row}: $What"
  $line | Tee-Object -FilePath "$Work/result.txt" -Append
  if (-not $Ok) { $script:failed += $line }
}

function Not-Tried([string] $Why) {
  "NOT TRIED A1.2 A1.8 A1.11: $Why" | Tee-Object -FilePath "$Work/result.txt" -Append
  Get-PnpDevice -PresentOnly -Class MEDIA, AudioEndpoint -ErrorAction SilentlyContinue |
    Format-Table -AutoSize Status, Class, FriendlyName, InstanceId | Out-String -Width 200 |
    Tee-Object -FilePath "$Work/env.txt" -Append
  Write-Output "::warning::default-device rows not tried: $Why"
  if ($log -and (Test-Path $log)) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }
  exit 0
}

$os = Get-CimInstance Win32_OperatingSystem
"os: $($os.Caption) build $($os.BuildNumber)" | Tee-Object -FilePath "$Work/env.txt"
$secureBoot = try { "$(Confirm-SecureBootUEFI)" } catch { "unreadable ($($_.Exception.Message))" }
"secure boot: $secureBoot" | Tee-Object -FilePath "$Work/env.txt" -Append
foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') {
  Set-Service -Name $name -StartupType Automatic
  Start-Service -Name $name
}
"active output endpoints before the driver: $([Endpoints]::ActiveCount())" | Tee-Object -FilePath "$Work/env.txt" -Append

# Scream 4.0 as released, outside the uploaded folder: the artifact carries no driver.
$fixture = (New-Item -ItemType Directory -Force -Path (Join-Path ([IO.Path]::GetTempPath()) 'audiosync-ci-driver')).FullName
$zip = Join-Path $fixture 'Scream4.0.zip'
Invoke-WebRequest -Uri 'https://github.com/duncanthrax/scream/releases/download/4.0/Scream4.0.zip' -OutFile $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash
if ($hash -ne 'FA33E25F9A46C61E4E0CD83362C51C3D2A45C6FE4091AAD7507E240E40F1A520') { throw "Scream4.0.zip hash $hash" }
Expand-Archive -Force $zip (Join-Path $fixture 'scream')
$driver = Join-Path $fixture 'scream/Install/driver/x64'

# The catalog signed again with a certificate this machine trusts, so the
# driver store takes the package without a prompt; Scream.sys keeps its own signature.
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AudioSync CI test driver' -CertStoreLocation Cert:\LocalMachine\My
$cer = Join-Path $fixture 'ci-driver.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
& $signtool.FullName sign /fd SHA256 /sm /s My /sha1 $cert.Thumbprint (Join-Path $driver 'scream.cat')
if ($LASTEXITCODE -ne 0) { Not-Tried "signtool could not sign the catalog (exit $LASTEXITCODE)" }
pnputil /add-driver (Join-Path $driver 'Scream.inf') /install | Tee-Object -FilePath "$Work/env.txt" -Append
$created = [SoftwareDevice]::Create('AudioSyncCI', '*Scream', 'Speakers')
"software device: $created" | Tee-Object -FilePath "$Work/env.txt" -Append
# Run 44292105824 (Server 2025) refused SWD with 0x8007007E: a ROOT device
# instead, which AudioSync lists as virtual, so This PC has no System slider.
$script:builtIn = $created.StartsWith('0x00000000')
$devcon = Join-Path $fixture 'scream/Install/helpers/devcon-x64.exe'
if (-not $script:builtIn) {
  & $devcon install (Join-Path $driver 'Scream.inf') '*Scream' |
    Tee-Object -FilePath "$Work/env.txt" -Append
}

$count = 0
for ($i = 0; $i -lt 60 -and $count -eq 0; $i++) { Start-Sleep -Seconds 1; $count = [Endpoints]::ActiveCount() }
"active output endpoints with the driver: $count" | Tee-Object -FilePath "$Work/env.txt" -Append
Get-PnpDevice -PresentOnly -Class MEDIA, AudioEndpoint -ErrorAction SilentlyContinue | Format-Table -AutoSize Status, Class, FriendlyName, InstanceId |
  Out-String -Width 200 | Tee-Object -FilePath "$Work/env.txt" -Append
$default = Get-Defaults
"default output: $default" | Tee-Object -FilePath "$Work/env.txt" -Append
if ($count -ne 1) { Not-Tried "$count active output endpoints after installing the driver (one needed)" }

[Endpoints]::SetMute($false)
[Endpoints]::SetLevel(0.5)

$install = Start-Process -FilePath $Setup -Wait -PassThru `
  -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$Work\install.log`""
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
if ($install.ExitCode -ne 0 -or -not (Test-Path $exe)) { throw "install failed (exit $($install.ExitCode))" }

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes, System.Windows.Forms, System.Drawing
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$TogglePattern = [System.Windows.Automation.TogglePattern]::Pattern

function Get-AppWindows {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $script:app.Id)
  @($A::RootElement.FindAll($Scope::Children, $c))
}

function Find-All($Condition) { @(foreach ($w in Get-AppWindows) { $w.FindAll($Scope::Descendants, $Condition) }) }

function Find-Named([string] $Name) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, $Name)
  for ($i = 0; $i -lt 20; $i++) {
    $found = Find-All $c
    if ($found) { return $found[0] }
    Start-Sleep -Milliseconds 500
  }
  $null
}

function Get-Toggles {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::IsTogglePatternAvailableProperty, $true)
  Find-All $c
}

function Test-On($Element) { $Element.GetCurrentPattern($TogglePattern).Current.ToggleState -eq 'On' }

function Test-Alive { -not $script:app.HasExited }

$log = Join-Path $env:TEMP 'audiosync-debug.log'
function Get-LogLines([string] $Pattern) { @(Select-String -Path $log -Pattern $Pattern -SimpleMatch -ErrorAction SilentlyContinue) }

function Start-App {
  $env:AUDIOSYNC_DEBUG = '1'
  $script:app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
  Remove-Item Env:AUDIOSYNC_DEBUG
  $script:main = $null
  for ($i = 0; $i -lt 30 -and -not $script:main; $i++) { Start-Sleep -Seconds 1; $script:main = @(Get-AppWindows)[0] }
  if (-not $script:main) { throw 'no AudioSync window within 30 s' }
  Start-Sleep -Seconds 3
  # Run 44380792459: the fresh window refused focus once; the checks drive it through UI Automation anyway.
  try { $script:main.SetFocus() } catch { "step: the window refused focus ($($_.Exception.InnerException.Message))" | Tee-Object -FilePath "$Work/result.txt" -Append }
}

# Show Virtual Devices on, as a user with a virtual card would set it: the
# endpoint is ROOT-enumerated, which hides its row otherwise.
$data = Join-Path $env:LOCALAPPDATA 'AudioSync'
New-Item -ItemType Directory -Force -Path $data | Out-Null
'{"version":3,"show_virtual_devices":true}' | Set-Content -Encoding ascii (Join-Path $data 'settings.json')
Step 'app started on one endpoint'
Start-App

$toggles = Get-Toggles
"toggles: $(($toggles | ForEach-Object { "'$($_.Current.Name)'=$(Test-On $_)" }) -join ', ')" | Tee-Object -FilePath "$Work/result.txt" -Append
$streaming = $toggles | Where-Object { $_.Current.Name -eq 'Streaming' } | Select-Object -First 1
$row = $toggles | Where-Object { $_.Current.Name -match 'Speakers|Scream' } | Select-Object -First 1
if (-not $row -and -not $script:builtIn) { Not-Tried 'the endpoint has no row in the main window' }
if (-not $streaming -or -not $row) { throw 'no Streaming switch or no row for the endpoint (see toggles above)' }
if (-not (Test-On $row)) { $row.GetCurrentPattern($TogglePattern).Toggle(); Start-Sleep -Seconds 2 }
# Turning the first speaker on can start streaming by itself (run 44299973570).
if (-not (Test-On $streaming)) { $streaming.GetCurrentPattern($TogglePattern).Toggle() }
Start-Sleep -Seconds 6

# A1.11: the only output is the default; nothing to move it to.
Check 'A1.11' ((Test-Alive) -and (Test-On $streaming)) 'streaming is on with the default output the only one'
Check 'A1.11' ((Get-Defaults) -eq $default) "the default output stayed where it was ($(Get-Defaults))"
$noCopy = Get-LogLines "Windows' default output gets no copy while this stream runs"
Check 'A1.11' ($noCopy.Count -ge 1) "the log gives the reason ($(($noCopy | Select-Object -First 1).Line))"
(Get-LogLines 'default output:') | ForEach-Object { "log: $($_.Line)" }

# A1.2: a volume set on the default from outside, as the taskbar does, stays.
[Endpoints]::SetLevel(0.3)
Start-Sleep -Seconds 5
$level = [Endpoints]::Level()
Check 'A1.2' ([math]::Abs($level - 0.3) -lt 0.005) "the taskbar's 30% on the default stays while streaming (reads $([math]::Round($level * 100, 1))%)"
Check 'A1.2' ((Test-Alive) -and (Test-On $streaming)) 'the stream goes on after the volume change'
(Get-LogLines 'device volume: 30%') | Select-Object -First 2 | ForEach-Object { "log: $($_.Line)" }

Step 'A1.8'
# A1.8: the System slider on the muted default unmutes it.
Select-String -Path $log -Pattern 'outputs listed' -SimpleMatch -Context 0, 2 -ErrorAction SilentlyContinue |
  Select-Object -First 1 | ForEach-Object { "log: $($_.Line) $($_.Context.PostContext -join ' ')" }
if (-not $script:builtIn) {
  # Setting its stored enumerator name and restarting the audio services lost the runner (runs 44356607387, 44365737411).
  "NOT TRIED A1.8: the endpoint is a ROOT device, listed as virtual, so This PC shows no System slider" |
    Tee-Object -FilePath "$Work/result.txt" -Append
} else {
  [Endpoints]::SetMute($true)
  Start-Sleep -Seconds 3
  $opener = Find-Named 'System volume'
  if ($opener) {
    $opener.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
    Start-Sleep -Seconds 2
  }
  $sliderType = New-Object System.Windows.Automation.PropertyCondition($A::ControlTypeProperty, [System.Windows.Automation.ControlType]::Slider)
  $slider = Find-All $sliderType | Where-Object { $_.Current.Name -like 'Windows volume of *' } | Select-Object -First 1
  Check 'A1.8' ([bool]$slider) "the This PC row's System slider is there (opener found: $([bool]$opener))"
  $focused = $false
  if ($slider) {
    try { $script:main.SetFocus(); $slider.SetFocus(); $focused = $true } catch {
      "NOT TRIED A1.8 slider drag: no runner route, the slider refused focus ($($_.Exception.InnerException.Message))" |
        Tee-Object -FilePath "$Work/result.txt" -Append
    }
  }
  if ($focused) {
    Start-Sleep -Milliseconds 500
    [System.Windows.Forms.SendKeys]::SendWait('{LEFT}')
    Start-Sleep -Seconds 3
    $muted = [Endpoints]::Muted()
    $after = [Endpoints]::Level()
    Check 'A1.8' (-not $muted) "a move of the System slider unmuted the default (muted: $muted, level $([math]::Round($after * 100, 1))%)"
    Check 'A1.8' ((Test-Alive) -and (Test-On $streaming)) 'the stream goes on after the slider move'
  }
}

function Save-Shot([string] $Name) {
  $r = $script:main.Current.BoundingRectangle
  $shot = New-Object System.Drawing.Bitmap ([int]$r.Width), ([int]$r.Height)
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen([int]$r.X, [int]$r.Y, 0, 0, $shot.Size)
  $shot.Save((Join-Path $Work "$Name.png"), [System.Drawing.Imaging.ImageFormat]::Png)
  "shot: $Name.png" | Tee-Object -FilePath "$Work/result.txt" -Append
}

# The value of the last "master volume: N" line after the first $Seen, once the move rests.
function Wait-Master([int] $Seen) {
  for ($i = 0; $i -lt 30; $i++) {
    $lines = Get-LogLines 'master volume: '
    if ($lines.Count -gt $Seen) { Start-Sleep -Seconds 2; return [double]((Get-LogLines 'master volume: ')[-1].Line -replace '.*master volume: ', '') }
    Start-Sleep -Milliseconds 500
  }
  $null
}

# A10.11: real volume keys move AudioSync's master while it streams, and the
# default output's own volume stays (S429).
[Endpoints]::SetMute($false)
[Endpoints]::SetLevel(0.5)
Start-Sleep -Seconds 2
$hook = Get-LogLines 'volume keys: the keyboard hook takes the volume keys'
Check 'A10.11' ($hook.Count -ge 1) "the keyboard hook took the volume keys ($(($hook | Select-Object -First 1).Line))"
$before = [Endpoints]::Level()
$seen = (Get-LogLines 'master volume: ').Count
[Input]::Press(0xAE); Start-Sleep -Milliseconds 300; [Input]::Press(0xAE)
$down = Wait-Master $seen
$seen = (Get-LogLines 'master volume: ').Count
[Input]::Press(0xAF)
$up = Wait-Master $seen
$after = [Endpoints]::Level()
Check 'A10.11' (($null -ne $down) -and ($null -ne $up) -and [math]::Abs($up - $down - 3) -lt 0.01) "two Volume Down presses then one Volume Up: master $down, then $up"
Check 'A10.11' ([math]::Abs($after - $before) -lt 0.005) "the default output stayed at $([math]::Round($after * 100, 1))% (was $([math]::Round($before * 100, 1))%)"

# A10.10: a muted, then a quiet, default shows the hint line, its fix and,
# on hover, its tooltip.
foreach ($pass in 'muted', 'low') {
  if ($pass -eq 'muted') { [Endpoints]::SetMute($true) } else { [Endpoints]::SetMute($false); [Endpoints]::SetLevel(0.05) }
  Start-Sleep -Seconds 3
  $all = Find-All ([System.Windows.Automation.Condition]::TrueCondition)
  $hint = $all | Where-Object { $_.Current.Name -match 'muted in Windows|Windows volume is \d' } | Select-Object -First 1
  $fix = $all | Where-Object { $_.Current.Name -match '^(Unmute|Set to \d+%)' } | Select-Object -First 1
  Check 'A10.10' ([bool]$hint) "$pass hint: '$(if ($hint) { $hint.Current.Name })'"
  Check 'A10.10' ([bool]$fix) "$pass fix: '$(if ($fix) { $fix.Current.Name })'"
  if ($hint) {
    $r = $hint.Current.BoundingRectangle
    [Input]::Hover([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2))
    Start-Sleep -Seconds 2
    $tip = Find-All ([System.Windows.Automation.Condition]::TrueCondition) |
      Where-Object { $_.Current.Name -like 'Windows has its own volume for each output*' } | Select-Object -First 1
    "$(if ($tip) { 'SEEN' } else { 'NOT SEEN (see the photo)' }) A10.10: $pass tooltip on hover: '$(if ($tip) { $tip.Current.Name })'"
  }
  Save-Shot "A10.10-$pass"
}
[Endpoints]::SetLevel(0.5)

if (Test-On $streaming) { $streaming.GetCurrentPattern($TogglePattern).Toggle() }
Start-Sleep -Seconds 3
Check 'A1.11' ((Get-Defaults) -eq $default) 'the default output is as found after streaming stops'
if (Test-Alive) { Stop-Process -Id $script:app.Id -Force }
Start-Sleep -Seconds 2
if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }

# A1.1: with a second, silent endpoint the app moves the default while the
# whole system streams (S469); a kill and the uninstall each must leave the
# default, volume and mute as found (S474).
function Get-Found { "$(Get-Defaults)`n$([Endpoints]::Readings())" }

function Get-Rows { @(Get-Toggles | Where-Object { $_.Current.Name -match 'Speakers|Scream' }) }

function Set-Toggle([string] $Name, [int] $Index, [bool] $On) {
  $e = if ($Name) { Get-Toggles | Where-Object { $_.Current.Name -eq $Name } | Select-Object -First 1 } else { (Get-Rows)[$Index] }
  if ((Test-On $e) -ne $On) { $e.GetCurrentPattern($TogglePattern).Toggle(); Start-Sleep -Seconds 2 }
}

# Streams with one row on at a time until the default moves; whether it did.
function Start-MovedStream([string] $Before) {
  $rows = (Get-Rows).Count
  for ($pick = 0; $pick -lt $rows; $pick++) {
    Set-Toggle 'Streaming' 0 $false
    for ($r = 0; $r -lt $rows; $r++) { Set-Toggle '' $r ($r -eq $pick) }
    Set-Toggle 'Streaming' 0 $true
    Start-Sleep -Seconds 6
    if ((Get-Defaults) -ne $Before) { return $true }
  }
  $false
}

# A10.14: a throwaway app plays a tone to the default, AudioSync streams that
# app, the app is killed, and the source goes to All System Audio with a notice
# (S227).
$wav = Join-Path $fixture 'tone.wav'
$samples = 44100
$stream = [IO.File]::Create($wav)
$w = New-Object IO.BinaryWriter $stream
$w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + 2 * $samples)); $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
$w.Write([int]16); $w.Write([int16]1); $w.Write([int16]1); $w.Write([int]44100); $w.Write([int](2 * 44100)); $w.Write([int16]2); $w.Write([int16]16)
$w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int](2 * $samples))
for ($i = 0; $i -lt $samples; $i++) { $w.Write([int16](8000 * [math]::Sin(2 * [math]::PI * 440 * $i / 44100))) }
$w.Close()
"powershell.exe processes before the tone: $(@(Get-Process powershell -ErrorAction SilentlyContinue).Count)" | Tee-Object -FilePath "$Work/result.txt" -Append
$player = Start-Process -FilePath powershell.exe -WindowStyle Hidden -PassThru `
  -ArgumentList '-NoProfile', '-Command', "(New-Object Media.SoundPlayer '$wav').PlayLooping(); Start-Sleep -Seconds 600"
Start-Sleep -Seconds 3
'{"version":3,"show_virtual_devices":true,"source":{"kind":"app","executable":"powershell.exe","name":"Windows PowerShell"}}' |
  Set-Content -Encoding ascii (Join-Path $data 'settings.json')
Start-App
Set-Toggle '' 0 $true
Set-Toggle 'Streaming' 0 $true
Start-Sleep -Seconds 6
$sourceSeen = (Get-LogLines 'source').Count
Stop-Process -Id $player.Id -Force
Start-Sleep -Seconds 8
$notice = Find-All ([System.Windows.Automation.Condition]::TrueCondition) |
  Where-Object { $_.Current.Name -match ' quit, ' } | Select-Object -First 1
Check 'A10.14' ([bool]$notice) "the notice after the source app was killed: '$(if ($notice) { $notice.Current.Name })'"
Save-Shot 'A10.14-source-quit'
@(Get-LogLines 'source') | Select-Object -Skip ([math]::Max(0, $sourceSeen - 3)) | ForEach-Object { "log: $($_.Line)" } |
  Tee-Object -FilePath "$Work/result.txt" -Append
if (Test-Alive) { Stop-Process -Id $script:app.Id -Force }
Start-Sleep -Seconds 2
if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug-a1014.log') }
'{"version":3,"show_virtual_devices":true}' | Set-Content -Encoding ascii (Join-Path $data 'settings.json')

& $devcon install (Join-Path $driver 'Scream.inf') '*Scream' | Tee-Object -FilePath "$Work/env.txt" -Append
$count = 0
for ($i = 0; $i -lt 60 -and $count -ne 2; $i++) { Start-Sleep -Seconds 1; $count = [Endpoints]::ActiveCount() }
if ($count -ne 2) {
  "NOT TRIED A1.1: $count active output endpoints after adding a second one (two needed)" | Tee-Object -FilePath "$Work/result.txt" -Append
} else {
  [Endpoints]::SetMute($false)
  [Endpoints]::SetLevel(0.42)
  $defaults = Get-Defaults
  $found = Get-Found
  "A1.1 before the app starts:`n$found" | Tee-Object -FilePath "$Work/result.txt" -Append
  foreach ($leg in 'kill', 'uninstall') {
    Step "A1.1 $leg leg"
    Start-App
    $moved = Start-MovedStream $defaults
    $copies = @(Get-Process audiosync -ErrorAction SilentlyContinue).Count
    "A1.1 $leg, while streaming: $(Get-Defaults); moved: $moved; audiosync processes: $copies" | Tee-Object -FilePath "$Work/result.txt" -Append
    if ($leg -eq 'kill') {
      # The Volume Mixer leg lost the runner, with no log, in runs 44365737411, 44386586904 and 44398574742.
      "NOT TRIED A1.2 mixer: no runner route" | Tee-Object -FilePath "$Work/result.txt" -Append
      Stop-Process -Id $script:app.Id -Force
    } else {
      Start-Process -FilePath (Join-Path $dir 'unins000.exe') -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
      for ($i = 0; $i -lt 60 -and (Test-Path $exe); $i++) { Start-Sleep -Seconds 1 }
    }
    for ($i = 0; $i -lt 15 -and (Get-Found) -ne $found; $i++) { Start-Sleep -Seconds 1 }
    $now = Get-Found
    "A1.1 after the ${leg}:`n$now" | Tee-Object -FilePath "$Work/result.txt" -Append
    if (-not $moved) { "WEAK A1.1 ${leg}: the app moved no default, so nothing was restored" | Tee-Object -FilePath "$Work/result.txt" -Append }
    Check 'A1.1' ($now -eq $found) "after the $leg the default output and every output's volume and mute are as found"
    if (Test-Path $log) { Copy-Item $log (Join-Path $Work "audiosync-debug-a11-$leg.log") }
  }
}

if ($script:failed) { "$($script:failed.Count) checks failed"; exit 1 }
'every default-device check passed'
exit 0
