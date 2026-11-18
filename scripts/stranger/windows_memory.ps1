# A stranger's Windows PC streaming for five minutes, run by
# .github/workflows/stranger.yml one phase per step, so a runner that freezes
# still shows the phase it froze in: two Scream virtual outputs (MS-PL,
# downloaded and checked by hash, never shipped) as the PC's speakers, a tone
# on both for 60 s without AudioSync, a silent install of AudioSync-Setup.exe,
# a launch from Program Files with a tone playing to the default while
# AudioSync streams it, the private bytes and the system's available memory
# sampled every 10 s, then the uninstall while the app runs, and the default
# output and every output's volume and mute compared to the ones found before
# the install. The phases share state through files in the work folder.
#
# Memory bound: after the first minute (buffers, fonts and the GPU settle),
# private bytes may rise at most 1 MB a minute (least-squares slope over
# minutes 1 to 5). A per-callback leak of one 10 ms buffer is about 20 MB a
# minute; slower leaks are left to the 60-minute soak.
#
#   pwsh scripts/stranger/windows_memory.ps1 -Phase <phase> -Work <folder>
#        [-Setup <AudioSync-Setup.exe>] [-From <s> -To <s>]
param(
  [Parameter(Mandatory)] [ValidateSet('scream', 'tone', 'install', 'launch', 'sample', 'uninstall')] [string] $Phase,
  [Parameter(Mandatory)] [string] $Work,
  [string] $Setup,
  [int] $From = 0,
  [int] $To = 300,
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

public static class Machine {
  [StructLayout(LayoutKind.Sequential)]
  class MemoryStatus {
    public uint Length = 64, Load;
    public ulong TotalPhys, AvailPhys, TotalPage, AvailPage, TotalVirtual, AvailVirtual, AvailExtended;
  }
  [DllImport("kernel32.dll")] static extern bool GlobalMemoryStatusEx([In, Out] MemoryStatus status);

  public static ulong Available() {
    var status = new MemoryStatus();
    return GlobalMemoryStatusEx(status) ? status.AvailPhys : 0;
  }

  [StructLayout(LayoutKind.Sequential)]
  struct WaveFormat { public short Tag, Channels; public int Rate, BytesPerSecond; public short Align, Bits, Extra; }
  [StructLayout(LayoutKind.Sequential)]
  struct WaveHeader { public IntPtr Data; public int Length, Recorded; public IntPtr User; public int Flags, Loops; public IntPtr Next, Reserved; }
  [DllImport("winmm.dll")] static extern int waveOutGetNumDevs();
  [DllImport("winmm.dll")] static extern int waveOutOpen(out IntPtr handle, int device, ref WaveFormat format, IntPtr callback, IntPtr instance, int flags);
  [DllImport("winmm.dll")] static extern int waveOutPrepareHeader(IntPtr handle, IntPtr header, int size);
  [DllImport("winmm.dll")] static extern int waveOutWrite(IntPtr handle, IntPtr header, int size);

  // A 440 Hz tone of the given length started on every waveOut device at
  // once, without the default in between; "<device>=<open>/<write>" each.
  public static string ToneEverywhere(int seconds) {
    var format = new WaveFormat { Tag = 1, Channels = 1, Rate = 44100, BytesPerSecond = 88200, Align = 2, Bits = 16 };
    int samples = 44100 * seconds;
    IntPtr data = Marshal.AllocHGlobal(samples * 2);
    for (int i = 0; i < samples; i++) Marshal.WriteInt16(data, i * 2, (short)(8000 * Math.Sin(2 * Math.PI * 440 * i / 44100.0)));
    int size = Marshal.SizeOf(typeof(WaveHeader));
    var lines = new System.Text.StringBuilder();
    int count = waveOutGetNumDevs();
    for (int device = 0; device < count; device++) {
      IntPtr handle;
      int open = waveOutOpen(out handle, device, ref format, IntPtr.Zero, IntPtr.Zero, 0);
      int write = -1;
      if (open == 0) {
        IntPtr header = Marshal.AllocHGlobal(size);
        Marshal.StructureToPtr(new WaveHeader { Data = data, Length = samples * 2 }, header, false);
        write = waveOutPrepareHeader(handle, header, size);
        if (write == 0) write = waveOutWrite(handle, header, size);
      }
      lines.AppendFormat("{0}={1}/{2} ", device, open, write);
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

# A UTC time and the seconds since the start on every phase and sample, so a
# hang shows where it stopped.
$script:clock = [Diagnostics.Stopwatch]::StartNew()
function Say([string] $What) {
  '{0:HH:mm:ss}Z +{1}s {2}' -f [DateTime]::UtcNow, [int]$script:clock.Elapsed.TotalSeconds, $What | Tee-Object -FilePath "$Work/progress.txt" -Append | Out-Host
}

# UI Automation and the audio service's COM calls have blocked forever on CI
# runners: each runs on its own runspace and is abandoned past its limit.
function Bounded([string] $What, [scriptblock] $Block, [hashtable] $With = @{}, [int] $Seconds = 60) {
  $state = [initialsessionstate]::CreateDefault2()
  foreach ($f in Get-Item function:Get-Found, function:Get-Toggles, function:Test-On -ErrorAction SilentlyContinue) {
    $state.Commands.Add([Management.Automation.Runspaces.SessionStateFunctionEntry]::new($f.Name, $f.Definition))
  }
  $vars = @{ app = $script:app; A = $script:A; Scope = $script:Scope; TogglePattern = $script:TogglePattern } + $With
  foreach ($k in $vars.Keys) { $state.Variables.Add([Management.Automation.Runspaces.SessionStateVariableEntry]::new($k, $vars[$k], '')) }
  $ps = [powershell]::Create($state)
  $null = $ps.AddScript("`$ErrorActionPreference = 'Stop'`n" + $Block.ToString())
  $call = $ps.BeginInvoke()
  if (-not $call.AsyncWaitHandle.WaitOne($Seconds * 1000)) {
    $null = $ps.BeginStop($null, $null)
    throw "$What did not return within $Seconds s"
  }
  try { $ps.EndInvoke($call) } finally { $ps.Dispose() }
}

# A process waited on by itself, not with its children, and killed past its limit.
function Invoke-Exe([string] $What, [string] $File, [string[]] $Arguments, [int] $Seconds = 180) {
  Say "start: $What"
  $out = Join-Path $Work 'exe-output.txt'
  $p = Start-Process -FilePath $File -ArgumentList $Arguments -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError "$out.err"
  $null = $p.Handle
  if (-not $p.WaitForExit($Seconds * 1000)) {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    throw "$What did not finish within $Seconds s"
  }
  Get-Content $out, "$out.err" | Tee-Object -FilePath "$Work/env.txt" -Append | Out-Host
  Say "done: $What (exit $($p.ExitCode))"
  $p.ExitCode
}

# The app and the tone player outlive a phase's step (the runner stops them at
# the job's end); their ids and the launch time pass on in state.json.
$statePath = Join-Path $Work 'state.json'
$state = if (Test-Path $statePath) { Get-Content -Raw $statePath | ConvertFrom-Json -AsHashtable } else { @{} }
function Save-State { $state | ConvertTo-Json | Set-Content -Encoding utf8 $statePath }
function Get-Running([string] $Key) { if ($state[$Key]) { Get-Process -Id $state[$Key] -ErrorAction SilentlyContinue } }
$script:app = Get-Running 'app'
$script:player = Get-Running 'player'

$fixture = (New-Item -ItemType Directory -Force -Path (Join-Path ([IO.Path]::GetTempPath()) 'audiosync-ci-memory')).FullName
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
$log = Join-Path $env:TEMP 'audiosync-debug.log'
$foundPath = Join-Path $Work 'found.txt'
$csv = Join-Path $Work 'memory.csv'

# Each row lands in the step summary and in memory.csv the moment it is read,
# so a runner that dies mid-run still leaves the curve up to there.
function Write-Summary([string] $Line) { if ($env:GITHUB_STEP_SUMMARY) { $Line | Add-Content -Encoding utf8 $env:GITHUB_STEP_SUMMARY } }
function Get-AvailableMB { [math]::Round([Machine]::Available() / 1MB) }

# On a failure the player and the app stop, so neither outlives the job, and
# the debug log is kept.
trap {
  Say "stopped: $_"
  foreach ($p in $script:player, $script:app) { if ($p) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
  if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }
  [Environment]::Exit(1)
}

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
# Every switch as "<on>|<name>".
function Read-Toggles { @(Bounded 'read the switches' { foreach ($t in Get-Toggles) { "$(Test-On $t)|$($t.Current.Name)" } }) }
function Test-Streaming { [bool](Bounded 'read the Streaming switch' { [bool]@(Get-Toggles | Where-Object { $_.Current.Name -eq 'Streaming' -and (Test-On $_) }).Count }) }

switch ($Phase) {
  'scream' {
    Say 'start the audio services'
    $os = Get-CimInstance Win32_OperatingSystem -OperationTimeoutSec 60
    "os: $($os.Caption) build $($os.BuildNumber), $(Get-AvailableMB) MB available" | Tee-Object -FilePath "$Work/env.txt"
    foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') {
      Bounded "start $name" { Set-Service -Name $name -StartupType Automatic; Start-Service -Name $name } @{ name = $name } 120
    }

    # Scream 4.0 as windows_default_device.ps1 installs it: the catalog signed
    # again with a certificate this machine trusts; two ROOT devices, two outputs.
    Say 'download Scream'
    $zip = Join-Path $fixture 'Scream4.0.zip'
    Invoke-WebRequest -Uri 'https://github.com/duncanthrax/scream/releases/download/4.0/Scream4.0.zip' -OutFile $zip -TimeoutSec 120
    $hash = (Get-FileHash -Algorithm SHA256 $zip).Hash
    if ($hash -ne 'FA33E25F9A46C61E4E0CD83362C51C3D2A45C6FE4091AAD7507E240E40F1A520') { throw "Scream4.0.zip hash $hash" }
    Expand-Archive -Force $zip (Join-Path $fixture 'scream')
    $driver = Join-Path $fixture 'scream/Install/driver/x64'
    $inf = "`"$(Join-Path $driver 'Scream.inf')`""
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AudioSync CI test driver' -CertStoreLocation Cert:\LocalMachine\My
    $cer = Join-Path $fixture 'ci-driver.cer'
    Export-Certificate -Cert $cert -FilePath $cer | Out-Null
    foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
    $code = Invoke-Exe 'sign the Scream catalog' $signtool.FullName @('sign', '/fd', 'SHA256', '/sm', '/s', 'My', '/sha1', $cert.Thumbprint, "`"$(Join-Path $driver 'scream.cat')`"")
    if ($code -ne 0) { throw "signtool could not sign the catalog (exit $code)" }
    $null = Invoke-Exe 'add the Scream driver' pnputil.exe @('/add-driver', $inf, '/install')
    $devcon = Join-Path $fixture 'scream/Install/helpers/devcon-x64.exe'
    $count = 0
    for ($n = 1; $n -le 2; $n++) {
      $null = Invoke-Exe "add Scream output $n" $devcon @('install', $inf, '*Scream')
      for ($i = 0; $i -lt 60 -and $count -lt $n; $i++) { Start-Sleep -Seconds 1; $count = Bounded 'count the active outputs' { [Endpoints]::ActiveCount() } }
    }
    Say "active output endpoints: $count, $(Get-AvailableMB) MB available"
    "active output endpoints: $count" | Tee-Object -FilePath "$Work/env.txt" -Append
    if ($count -ne 2) { throw "$count active output endpoints after adding two (two needed)" }
  }

  'tone' {
    # Scream alone, no AudioSync: a freeze here is the driver's, not ours.
    Say 'a tone on every output for 60 s without AudioSync'
    $devices = [Machine]::ToneEverywhere(60)
    Say "waveOut device=open/write: $devices"
    Write-Summary "| seconds | available MB |`n|---|---|"
    for ($t = 0; $t -le 60; $t += 10) {
      Say "tone $t s: $(Get-AvailableMB) MB available"
      Write-Summary "| $t | $(Get-AvailableMB) |"
      if ($t -lt 60) { Start-Sleep -Seconds 10 }
    }
    Check ($devices -match '^0=0/0 1=0/0') "a tone played on both outputs for 60 s without AudioSync, the runner stayed up ($devices)"
  }

  'install' {
    Bounded 'set every output to 42% and unmuted' { [Endpoints]::SetAll(0.42) }
    $found = [string](Bounded 'read the outputs' { Get-Found })
    $found | Set-Content -Encoding utf8 -NoNewline $foundPath
    "before the install:`n$found" | Tee-Object -FilePath "$Work/result.txt" -Append

    $code = Invoke-Exe 'install AudioSync' $Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$Work\install.log`"")
    if ($code -ne 0 -or -not (Test-Path $exe)) { throw "install failed (exit $code)" }
    "installed: $exe $((Get-Item $exe).VersionInfo.ProductVersion)" | Tee-Object -FilePath "$Work/result.txt" -Append
  }

  'launch' {
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
    Say 'play the tone and launch AudioSync'
    $script:player = Start-Process -FilePath powershell.exe -WindowStyle Hidden -PassThru `
      -ArgumentList '-NoProfile', '-Command', "(New-Object Media.SoundPlayer '$wav').PlayLooping(); Start-Sleep -Seconds 1500"

    # Show Virtual Devices on: ROOT-enumerated outputs are listed as virtual.
    $data = Join-Path $env:LOCALAPPDATA 'AudioSync'
    New-Item -ItemType Directory -Force -Path $data | Out-Null
    '{"version":3,"show_virtual_devices":true}' | Set-Content -Encoding ascii (Join-Path $data 'settings.json')
    $env:AUDIOSYNC_DEBUG = '1'
    $script:app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
    Remove-Item Env:AUDIOSYNC_DEBUG
    $state.app = $app.Id
    $state.player = $player.Id
    $state.launched = [DateTime]::UtcNow.Ticks
    Save-State

    Say 'wait for the Streaming switch'
    $toggles = @()
    for ($i = 0; $i -lt 30 -and -not ($toggles -like '*|Streaming'); $i++) { Start-Sleep -Seconds 1; $toggles = Read-Toggles }
    Start-Sleep -Seconds 3
    $toggles = Read-Toggles
    "toggles: $(($toggles | ForEach-Object { $on, $name = $_ -split '\|', 2; "'$name'=$on" }) -join ', ')" | Tee-Object -FilePath "$Work/result.txt" -Append
    $rows = @($toggles | Where-Object { ($_ -split '\|', 2)[1] -match 'Speakers|Scream' })
    if (-not ($toggles -like '*|Streaming') -or $rows.Count -lt 2) { throw 'no Streaming switch or fewer than two Scream rows (see toggles above)' }
    Say 'switch the Scream rows and Streaming on'
    Bounded 'switch the Scream rows on' { foreach ($t in Get-Toggles) { if ($t.Current.Name -match 'Speakers|Scream' -and -not (Test-On $t)) { $t.GetCurrentPattern($TogglePattern).Toggle(); Start-Sleep -Seconds 2 } } }
    Bounded 'switch Streaming on' { foreach ($t in Get-Toggles) { if ($t.Current.Name -eq 'Streaming' -and -not (Test-On $t)) { $t.GetCurrentPattern($TogglePattern).Toggle() } } }
    Start-Sleep -Seconds 6
    $default = Bounded 'read the default output' { [Endpoints]::Default(0) }
    Check ((-not $app.HasExited) -and (Test-Streaming)) "streaming to both outputs (default while streaming: $default)"
  }

  'sample' {
    # Seconds since the launch, read from the clock, so the steps' own
    # overhead does not bend the slope.
    function Get-Since { [int](([DateTime]::UtcNow.Ticks - $state.launched) / 1e7) }
    if (-not (Test-Path $csv)) { 'seconds,private_bytes,available_bytes' | Set-Content -Encoding ascii $csv }
    Write-Summary "| seconds since launch | private MB | available MB |`n|---|---|---|"
    Say "sample every 10 s from $From s to $To s"
    function Measure-Once {
      if (-not $app -or $app.HasExited) { throw 'the app is no longer running' }
      $app.Refresh()
      $since = Get-Since
      $available = [Machine]::Available()
      "$since,$($app.PrivateMemorySize64),$available" | Add-Content -Encoding ascii $csv
      Write-Summary ('| {0} | {1:F1} | {2} |' -f $since, ($app.PrivateMemorySize64 / 1MB), [math]::Round($available / 1MB))
      Say ('sample {0} s: {1:F1} MB private, {2} MB available' -f $since, ($app.PrivateMemorySize64 / 1MB), [math]::Round($available / 1MB))
      # Stopped and failed here, before the runner freezes and takes the log with it.
      if ($available -lt 1GB) {
        Check $false ('the system had {0} MB available at {1} s with the app at {2:F1} MB private' -f [math]::Round($available / 1MB), $since, ($app.PrivateMemorySize64 / 1MB))
        throw 'the system ran low on memory'
      }
    }
    while ((Get-Since) -lt $To) { Measure-Once; Start-Sleep -Seconds 10 }
    if ($To -lt $Seconds) { exit 0 }
    Measure-Once

    Check ((-not $app.HasExited) -and (Test-Streaming)) "the app runs and streams after $Seconds s"
    $points = @(Import-Csv $csv | ForEach-Object { [pscustomobject]@{ T = [double]$_.seconds; B = [double]$_.private_bytes } })
    $settled = @($points | Where-Object { $_.T -ge 60 })
    $mx = ($settled | Measure-Object T -Average).Average
    $my = ($settled | Measure-Object B -Average).Average
    $sxy = 0.0; $sxx = 0.0
    foreach ($p in $settled) { $sxy += ($p.T - $mx) * ($p.B - $my); $sxx += ($p.T - $mx) * ($p.T - $mx) }
    $slope = if ($sxx -gt 0) { $sxy / $sxx * 60 / 1MB } else { [double]::NaN }
    $first = [math]::Round($points[0].B / 1MB, 1)
    $last = [math]::Round($points[-1].B / 1MB, 1)
    $line = "private bytes {0} MB at the start, {1} MB at {2} s; slope over minutes 1-5 {3:F2} MB/min (bound 1)" -f $first, $last, $points[-1].T, $slope
    Check ($settled.Count -ge 20 -and $slope -lt 1.0) $line
    Write-Summary "`n$line"
  }

  'uninstall' {
    if ($player) { Stop-Process -Id $player.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }

    # The uninstaller closes the running app itself.
    $null = Invoke-Exe 'uninstall AudioSync' (Join-Path $dir 'unins000.exe') @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART')
    for ($i = 0; $i -lt 60 -and (Test-Path $exe); $i++) { Start-Sleep -Seconds 1 }
    Check (-not (Test-Path $exe) -and (-not $app -or $app.HasExited)) 'uninstalled, and the app no longer runs'
    if ($app -and -not $app.HasExited) { Stop-Process -Id $app.Id -Force }
    Say 'compare the outputs with the ones found'
    $found = Get-Content -Raw $foundPath
    for ($i = 0; $i -lt 15 -and [string](Bounded 'read the outputs' { Get-Found }) -ne $found; $i++) { Start-Sleep -Seconds 1 }
    $now = [string](Bounded 'read the outputs' { Get-Found })
    "after the uninstall:`n$now" | Tee-Object -FilePath "$Work/result.txt" -Append
    Check ($now -eq $found) "after the uninstall the default output and every output's volume and mute are as found"

    $failed = @(Get-Content "$Work/result.txt" | Where-Object { $_ -like 'FAIL *' })
    if ($failed) { "$($failed.Count) checks failed"; exit 1 }
    'every memory and uninstall check passed'
  }
}

Say "phase $Phase done"
exit 0
