# The double-audio proof of docs/WINDOWS_SILENT_OUTPUT_SPEC.md (VC51-VC54),
# run by .github/workflows/silent-output-windows.yml on a clean runner.
# The runner's only output is the Scream sound card (MS-PL, checked by hash,
# runner only), sending what it plays to 127.0.0.1:4010; the receiver is the
# airplay crate's stranger_rx in a Windows container, since discovery drops
# this computer's own addresses (P234). A player plays heard.py's click train
# on the default output the whole time while AudioSync streams All System
# Audio to the receiver:
#   red (VC51): the PC and the receiver both hold the click train;
#   green (VC52): VB-CABLE added by AudioSync's own installer, run unattended
#     for everyone (VC25), or where there is none (ARM64) by Fix It on the
#     notice through UI Automation, or when neither can run by pnputil and
#     devcon; the player is then restarted, Scream holds the train once, in
#     sync with the receiver and with nothing at the red copy's offset, and
#     the receiver holds it once; a take with the first player still running
#     is recorded;
#   removal (VC53): AudioSync's uninstaller after the installer leg, logged,
#     never failed.
# The leg that produced the result goes into the job summary.
#
#   pwsh scripts/stranger/windows_silent_output.ps1 -Exe <audiosync.exe> -Rx <stranger_rx.exe> -Work <folder> [-Setup <AudioSync-Setup.exe>] [-Arch x64|arm64]
param(
  [Parameter(Mandatory)] [string] $Exe,
  [Parameter(Mandatory)] [string] $Rx,
  [Parameter(Mandatory)] [string] $Work,
  [string] $Setup = '',
  [ValidateSet('x64', 'arm64')] [string] $Arch = 'x64'
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$Work = (Resolve-Path $Work).Path
$Exe = (Resolve-Path $Exe).Path
$here = $PSScriptRoot

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

public static class Endpoints {
  static IMMDeviceEnumerator Enumerator() { return (IMMDeviceEnumerator)new MMDeviceEnumeratorClass(); }

  // flow 0 is render, 1 capture; role 0 eConsole, 1 eMultimedia, 2 eCommunications.
  public static string Default(int flow, int role) {
    IMMDevice device;
    int hr = Enumerator().GetDefaultAudioEndpoint(flow, role, out device);
    if (hr != 0) return string.Format("none (0x{0:X8})", hr);
    string id;
    hr = device.GetId(out id);
    return hr == 0 ? id : string.Format("unreadable (0x{0:X8})", hr);
  }

  public static int Count(int flow) {
    IMMDeviceCollection devices;
    if (Enumerator().EnumAudioEndpoints(flow, 1, out devices) != 0) return 0;
    uint count;
    return devices.GetCount(out count) == 0 ? (int)count : 0;
  }
}

// cfgmgr32's software device API (swdevice.h): a device under SWD\, which
// AudioSync lists as the PC's own speaker, not as virtual.
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

  public static string Create(string enumerator, string hardwareId, string description, uint flags) {
    var done = new ManualResetEvent(false);
    callback = (device, hr, context, id) => { result = hr; instance = id; done.Set(); };
    var info = new CreateInfo {
      cbSize = (uint)Marshal.SizeOf(typeof(CreateInfo)),
      pszInstanceId = "0",
      pszzHardwareIds = hardwareId + "\0",
      CapabilityFlags = flags,
      pszDeviceDescription = description,
    };
    int hr2 = SwDeviceCreate(enumerator, "HTREE\\ROOT\\0", ref info, 0, IntPtr.Zero, callback, IntPtr.Zero, out handle);
    if (hr2 != 0) return string.Format("0x{0:X8}: SwDeviceCreate failed", hr2);
    if (!done.WaitOne(120000)) return "timeout: no answer from PnP in 120 s";
    return string.Format("0x{0:X8}: {1}", result, instance);
  }
}

// A devnode under HTREE\ROOT\0 whose instance ID names its own enumerator
// rather than ROOT, registered and given the INF's driver by picking it from
// the class list: SetupAPI sets hardware IDs on ROOT devices only (0xE0000209).
public static class NamedDevnode {
  [StructLayout(LayoutKind.Sequential)]
  struct DevinfoData { public uint cbSize; public Guid ClassGuid; public uint DevInst; public IntPtr Reserved; }

  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct InstallParams {
    public uint cbSize; public uint Flags; public uint FlagsEx; public IntPtr hwndParent; public IntPtr InstallMsgHandler;
    public IntPtr InstallMsgHandlerContext; public IntPtr FileQueue; public UIntPtr ClassInstallReserved; public uint Reserved;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string DriverPath;
  }

  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
  struct DriverData {
    public uint cbSize; public uint DriverType; public UIntPtr Reserved;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string Description;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string MfgName;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string ProviderName;
    public uint DateLow; public uint DateHigh; public ulong DriverVersion;
  }

  [DllImport("setupapi.dll", SetLastError = true)]
  static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid classGuid, IntPtr parent);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiCreateDeviceInfoW(IntPtr set, string name, ref Guid classGuid, string description,
    IntPtr parent, uint flags, ref DevinfoData data);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiGetDeviceInstallParamsW(IntPtr set, ref DevinfoData data, ref InstallParams p);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiSetDeviceInstallParamsW(IntPtr set, ref DevinfoData data, ref InstallParams p);
  [DllImport("setupapi.dll", SetLastError = true)]
  static extern bool SetupDiBuildDriverInfoList(IntPtr set, ref DevinfoData data, uint type);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiEnumDriverInfoW(IntPtr set, ref DevinfoData data, uint type, uint index, ref DriverData driver);
  [DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool SetupDiSetSelectedDriverW(IntPtr set, ref DevinfoData data, ref DriverData driver);
  [DllImport("setupapi.dll", SetLastError = true)]
  static extern bool SetupDiCallClassInstaller(uint function, IntPtr set, ref DevinfoData data);
  [DllImport("newdev.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  static extern bool DiInstallDevice(IntPtr parent, IntPtr set, ref DevinfoData data, ref DriverData driver, uint flags, out bool reboot);

  static string Failed(string step) { return string.Format("{0} 0x{1:X8}", step, Marshal.GetLastWin32Error()); }

  public static string Create(string instanceId, string inf) {
    var media = new Guid("4d36e96c-e325-11ce-bfc1-08002be10318");
    IntPtr set = SetupDiCreateDeviceInfoList(ref media, IntPtr.Zero);
    var data = new DevinfoData { cbSize = (uint)Marshal.SizeOf(typeof(DevinfoData)) };
    if (!SetupDiCreateDeviceInfoW(set, instanceId, ref media, "Speakers", IntPtr.Zero, 0, ref data)) return Failed("create");
    var p = new InstallParams { cbSize = (uint)Marshal.SizeOf(typeof(InstallParams)) };
    if (!SetupDiGetDeviceInstallParamsW(set, ref data, ref p)) return Failed("install params");
    p.Flags |= 0x10000; // DI_ENUMSINGLEINF
    p.DriverPath = inf;
    if (!SetupDiSetDeviceInstallParamsW(set, ref data, ref p)) return Failed("driver path");
    if (!SetupDiBuildDriverInfoList(set, ref data, 1)) return Failed("driver list"); // SPDIT_CLASSDRIVER
    var driver = new DriverData { cbSize = (uint)Marshal.SizeOf(typeof(DriverData)) };
    if (!SetupDiEnumDriverInfoW(set, ref data, 1, 0, ref driver)) return Failed("no driver in the INF");
    if (!SetupDiSetSelectedDriverW(set, ref data, ref driver)) return Failed("select");
    if (!SetupDiCallClassInstaller(0x19, set, ref data)) return Failed("register"); // DIF_REGISTERDEVICE
    bool reboot;
    if (!DiInstallDevice(IntPtr.Zero, set, ref data, ref driver, 0, out reboot)) return Failed("install " + driver.Description);
    return "0x00000000: " + instanceId + ", " + driver.Description;
  }
}
'@

$summary = if ($env:GITHUB_STEP_SUMMARY) { $env:GITHUB_STEP_SUMMARY } else { Join-Path $Work 'summary.md' }
$script:failed = @()
function Note([string] $Line) { "$(Get-Date -Format HH:mm:ss) $Line" | Tee-Object -FilePath "$Work/result.txt" -Append }
function Check([string] $Row, [bool] $Ok, [string] $What) {
  $line = "$(if ($Ok) { 'PASS' } else { 'FAIL' }) ${Row}: $What"
  Note $line
  "- $line" | Out-File -Append -Encoding utf8 $summary
  if (-not $Ok) { $script:failed += $line }
}
function Get-Defaults {
  $roles = foreach ($flow in 0, 1) { foreach ($role in 0, 1, 2) { "$flow/$role=$([Endpoints]::Default($flow, $role))" } }
  $roles -join ' '
}
function Save-Devices([string] $When) {
  "--- $When" | Out-File -Append "$Work/env.txt"
  Get-PnpDevice -PresentOnly -Class MEDIA, AudioEndpoint -ErrorAction SilentlyContinue |
    Format-Table -AutoSize Status, Class, FriendlyName, InstanceId | Out-String -Width 200 | Out-File -Append "$Work/env.txt"
}
function Test-Cable { [bool](Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object { $_.HardwareID -contains 'VBAudioVACWDM' }) }
function Wait-For([scriptblock] $Test, [int] $Seconds) {
  for ($i = 0; $i -lt $Seconds; $i++) { if (& $Test) { return $true }; Start-Sleep -Seconds 1 }
  [bool](& $Test)
}

"## Silent output proof ($Arch)" | Out-File -Append -Encoding utf8 $summary
$os = Get-CimInstance Win32_OperatingSystem
Note "os: $($os.Caption) build $($os.BuildNumber), $Arch"
foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') { Set-Service -Name $name -StartupType Automatic; Start-Service -Name $name }
Set-NetFirewallProfile -All -Enabled False

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

# Scream unicast to the recorder on 127.0.0.1 rather than to its multicast group.
$options = 'HKLM:\SYSTEM\CurrentControlSet\Services\Scream\Options'
New-Item -Force -Path $options | Out-Null
Set-ItemProperty -Path $options -Name UnicastIPv4 -Value '127.0.0.1'
Set-ItemProperty -Path $options -Name UnicastPort -Value 4010 -Type DWord
$scream = Get-Checked 'https://github.com/duncanthrax/scream/releases/download/4.0/Scream4.0.zip' 'Scream4.0.zip' `
  'FA33E25F9A46C61E4E0CD83362C51C3D2A45C6FE4091AAD7507E240E40F1A520'
$driver = Join-Path $scream "Install/driver/$Arch"
$devcon = Join-Path $scream "Install/helpers/devcon-$Arch.exe"
# The catalog signed again with a certificate this machine trusts, as windows_default_device.ps1 does.
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=AudioSync CI test driver' -CertStoreLocation Cert:\LocalMachine\My
$cer = Join-Path $fixture 'ci-driver.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
foreach ($store in 'Root', 'TrustedPublisher') { Import-Certificate -FilePath $cer -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null }
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
& $signtool.FullName sign /fd SHA256 /sm /s My /sha1 $cert.Thumbprint (Join-Path $driver 'scream.cat')
if ($LASTEXITCODE -ne 0) { throw "signtool could not sign the Scream catalog (exit $LASTEXITCODE)" }
pnputil /add-driver (Join-Path $driver 'Scream.inf') /install | Out-File -Append "$Work/env.txt"
$created = [SoftwareDevice]::Create('AudioSyncCI', '*Scream', 'Speakers', 0x2 -bor 0x8) # SilentInstall, DriverRequired
Note "Scream software device: $created"
if (-not $created.StartsWith('0x00000000')) {
  # Run 44768767658: SWD gives 0x8007007E on the hosted images even for a device no driver matches.
  $created = [NamedDevnode]::Create('AUDIOSYNCCI\SCREAM\0000', (Join-Path $driver 'Scream.inf'))
  Note "Scream devnode with its own enumerator: $created"
}
if (-not $created.StartsWith('0x00000000')) { & $devcon install (Join-Path $driver 'Scream.inf') '*Scream' | Out-File -Append "$Work/env.txt" }
if (-not (Wait-For { [Endpoints]::Count(0) -ge 1 } 60)) { throw 'Scream gave no active output' }
# AudioSync reads the enumerator from the endpoint's store, which keeps a copy as
# PKEY_Device_EnumeratorName; a ROOT Scream is relabeled as the PC's own card.
$render = Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render' |
  ForEach-Object { Join-Path $_.PSPath 'Properties' }
$enumerator = '{a45c254e-df1c-4efd-8020-67d146a850e0},24'
$rooted = @($render | Where-Object { (Get-ItemProperty $_ -ErrorAction SilentlyContinue).$enumerator -eq 'ROOT' })
if ($rooted) {
  foreach ($key in $rooted) { Set-ItemProperty -Path $key -Name $enumerator -Value 'HDAUDIO' }
  Restart-Service -Force AudioEndpointBuilder
  Start-Service Audiosrv
  Note "relabeled $($rooted.Count) ROOT Scream endpoint(s) as HDAUDIO"
}
$bus = ($render | ForEach-Object { (Get-ItemProperty $_ -ErrorAction SilentlyContinue).$enumerator }) -join ','
Note "Scream endpoint enumerator: $bus"
if (-not $bus -or $bus -match 'ROOT') { throw "the Scream endpoint is still ROOT ($created, $bus), listed as virtual, so there is no double audio to prove" }
if (-not (Wait-For { [Endpoints]::Count(0) -ge 1 } 60)) { throw 'Scream gave no active output' }
Save-Devices 'with Scream'

$rxLog = Join-Path $Work 'receiver.log'
$rxDir = Split-Path -Parent (Resolve-Path $Rx).Path
# The Windows 11 ARM64 runner image ships no Docker, so the receiver has nowhere to run.
if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
  Note "untested: no Docker on the $Arch runner for the receiver container; Scream installed, nothing was checked"
  "- UNTESTED: no Docker on the $Arch runner for the receiver container" | Out-File -Append -Encoding utf8 $summary
  exit 0
}
docker run -d --name stranger-rx --isolation process -v "${rxDir}:C:\rx" -i mcr.microsoft.com/windows/nanoserver:ltsc2022 cmd | Out-Null
if ($LASTEXITCODE -ne 0) { throw "the receiver's container did not start (docker exit $LASTEXITCODE)" }
$rxIp = (docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' stranger-rx).Trim()
Note "receiver: stranger_rx in a container at $rxIp"
$receiver = Start-Process -FilePath cmd.exe -PassThru -WindowStyle Hidden -ArgumentList '/c', `
  "docker exec stranger-rx C:\rx\$(Split-Path -Leaf $Rx) $rxIp `"Stranger Receiver`" 2> `"$rxLog`" | python -I `"$here\heard.py`" record `"$Work\receiver.wav`" `"$Work\receiver.stamps`" --rate 44100"

# One click train long enough for the whole job, played once, so no loop seam breaks the cycle.
$clicks = Join-Path $Work 'clicks.wav'
python -I (Join-Path $here 'heard.py') clicks $clicks --seconds 1800 --rate 16000
# Media.SoundPlayer plays through waveOut (MME) on the device that is the default when it opens.
function Start-Player {
  Start-Process -FilePath powershell.exe -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile', '-Command', `
    "(New-Object Media.SoundPlayer '$clicks').PlaySync()"
}
$player = Start-Player

$log = Join-Path $env:TEMP 'audiosync-debug.log'
$env:AUDIOSYNC_DEBUG = '1'
$env:AUDIOSYNC_WEB_REMOTE = '1'
$env:AUDIOSYNC_WEB_REMOTE_PORT = '18731'
$app = Start-Process -FilePath $Exe -WorkingDirectory (Split-Path -Parent $Exe) -PassThru
python -I (Join-Path $here 'remote.py') $log 18731 'Stranger Receiver' | Tee-Object -FilePath "$Work/result.txt" -Append
if ($LASTEXITCODE -ne 0) {
  # Run 44772312990 got no pairing code here; the app's own log says why.
  Note "AudioSync exited: $($app.HasExited)$(if ($app.HasExited) { ", code $($app.ExitCode)" }); log present: $(Test-Path $log)"
  if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }
  throw 'the receiver could not be switched on through the web remote'
}
Start-Sleep -Seconds 6

function Test-Heard([string] $Phase) {
  $prefix = Join-Path $Work "scream-$Phase"
  python -I (Join-Path $here 'scream_window.py') record 4010 40 $prefix | Tee-Object -FilePath "$Work/result.txt" -Append | Out-Host
  $prefix
}

# VC51: today's behavior, the baseline.
$red = Test-Heard 'red'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$A = [System.Windows.Automation.AutomationElement]
$Scope = [System.Windows.Automation.TreeScope]
$Invoke = [System.Windows.Automation.InvokePattern]::Pattern
# The first element of a process's windows that can be invoked and whose name matches.
function Find-In([int] $ProcessId, [string] $Pattern) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $ProcessId)
  foreach ($w in $A::RootElement.FindAll($Scope::Children, $c)) {
    foreach ($e in $w.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) {
      $p = $null
      if ($e.Current.Name -match $Pattern -and $e.TryGetCurrentPattern($Invoke, [ref]$p)) { return $e }
    }
  }
  $null
}
function Get-Names([int] $ProcessId) {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::ProcessIdProperty, $ProcessId)
  @(foreach ($w in $A::RootElement.FindAll($Scope::Children, $c)) {
      $w.Current.Name
      foreach ($e in $w.FindAll($Scope::Descendants, [System.Windows.Automation.Condition]::TrueCondition)) { $e.Current.Name }
    }) | Where-Object { $_ } | Select-Object -Unique
}
function Use($Element) { $Element.GetCurrentPattern($Invoke).Invoke() }
# VC70: the one Windows dialog a user may meet after UAC; pressed as a user would, and logged.
function Approve-Dialog {
  $c = New-Object System.Windows.Automation.PropertyCondition($A::NameProperty, 'Windows Security')
  foreach ($w in $A::RootElement.FindAll($Scope::Children, $c)) {
    $yes = Find-In $w.Current.ProcessId '^Install$'
    if ($yes) { Use $yes; Note 'VC70: a Windows Security dialog asked to install the driver; Install pressed' }
  }
}
# VC40 from the app's own log: its defaults read before the install (VC30) and after its set-back.
function Get-AppDefaults([string] $When) {
  $line = Select-String -Path $log -Pattern "silent output: defaults $When the install (.*)" -ErrorAction SilentlyContinue | Select-Object -Last 1
  if ($line) { @([regex]::Matches($line.Matches[0].Groups[1].Value, 'Some\("([^"]*)"\)|None') | ForEach-Object { if ($_.Groups[1].Success) { $_.Groups[1].Value } else { 'none' } }) }
}

$beforeOutput = [Endpoints]::Default(0, 0)
Note "defaults before the install: $(Get-Defaults)"

# VC52: the installer's step (VC25), else Fix It on the notice, else pnputil and devcon.
$leg = 'unattended (pnputil and devcon)'
$why = ''
if ($Setup) {
  $leg = 'installer (unattended, for everyone)'
  $installer = Start-Process -FilePath (Resolve-Path $Setup).Path -PassThru -Wait `
    -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=$Work\setup.log"
  Note "installer exit code: $($installer.ExitCode)"
} elseif (Wait-For { ($script:button = Find-In $app.Id '^Fix It$') } 60) {
  Use $script:button
  $leg = 'Fix It (invoked through UI Automation)'
} else {
  $why = 'no installer and the notice showed no Fix It within 60 s'
}
Note "leg: $leg$(if ($why) { " ($why)" })"

if ($leg -notlike 'unattended*') {
  # VC34's wait: past it, a Windows dialog blocked the install (VC5).
  $added = Wait-For { Approve-Dialog | Out-Null; (Test-Cable) -and [Endpoints]::Count(0) -ge 2 } 600
  $vendor = @(Get-Process -Name 'VBCABLE_Setup*' -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle })
  Note "vendor windows during the install: $($vendor.Count)"
  Check 'VC52 install' ($added -and $vendor.Count -eq 0) 'VB-CABLE active within the 10-minute wait, no vendor window, no dialog in the way'
  # The step writes its after-line once VC40's 30 s guard has passed.
  Wait-For { Get-AppDefaults 'after' } 90 | Out-Null
  $appBefore = @(Get-AppDefaults 'before')
  $appAfter = @(Get-AppDefaults 'after')
  $slots = 'output console', 'output multimedia', 'output communications', 'input console', 'input multimedia', 'input communications'
  $vc40 = for ($i = 0; $i -lt 6; $i++) { "$($slots[$i]): $($appBefore[$i]) -> $($appAfter[$i])" }
  # A role with no default before has nothing to be set back to (VC40).
  $same = $appBefore.Count -eq 6 -and $appAfter.Count -eq 6 -and
    @(0..5 | Where-Object { $appBefore[$_] -ne $appAfter[$_] -and $appBefore[$_] -ne 'none' }).Count -eq 0
  Check 'VC40' $same "the defaults before the install equal those after it, per role: $($vc40 -join '; ')"
} else {
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
  Check 'VC52 unattended' (Wait-For { (Test-Cable) -and [Endpoints]::Count(0) -ge 2 } 60) 'VB-CABLE active within 60 s'
}
Note "defaults right after the install: $(Get-Defaults)"
Save-Devices 'with VB-CABLE'

# The app moves the default to the cable on its routing check (VC36).
$moved = Wait-For { [Endpoints]::Default(0, 0) -ne $beforeOutput } 60
Note "defaults with the app streaming: $(Get-Defaults) (moved: $moved)"
Start-Sleep -Seconds 4
$kept = Test-Heard 'kept'
# A player opened before the cable keeps its device, as any app may (VC52).
Stop-Process -Id $player.Id -Force -ErrorAction SilentlyContinue
$player = Start-Player
Start-Sleep -Seconds 4
$green = Test-Heard 'green'

# VC45: the cable is never a speaker row.
$rows = Get-Names $app.Id
$cableRows = @($rows | Where-Object { $_ -match 'CABLE' -and $_ -notmatch 'Fix It adds VB-CABLE' })
Check 'VC45' ($cableRows.Count -eq 0) "no cable row in the window$(if ($cableRows) { ": $($cableRows -join ', ')" })"

# VC53: the uninstaller removes the cable its step added; logged only, a runner can't restart.
$uninstaller = Join-Path $env:ProgramFiles 'AudioSync\unins000.exe'
if ($leg -like 'installer*' -and (Test-Path $uninstaller)) {
  Note "VC53: VB-CABLE folder beside the exe before uninstalling: $(Test-Path (Join-Path $env:ProgramFiles 'AudioSync\VB-CABLE'))"
  Start-Process -FilePath $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
  $gone = Wait-For { -not (Test-Cable) } 60
  Note "VC53: $(if ($gone) { 'the cable device went without a restart' } else { 'restart required: the cable device is still present' })"
  if ($gone) {
    $again = Wait-For { Find-In $app.Id '^Fix It$' } 60
    Note "VC53: Fix It back on the next routing check: $again"
  }
} else {
  Note "VC53: not run ($leg); VB-CABLE added this way stays until the user removes it (VC47)"
}

Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $player.Id -Force -ErrorAction SilentlyContinue
docker rm -f stranger-rx | Out-Null
$receiver.WaitForExit(15000) | Out-Null
if (Test-Path $log) { Copy-Item $log (Join-Path $Work 'audiosync-debug.log') }

$heard = Join-Path $here 'heard.py'
$window = Join-Path $here 'scream_window.py'
foreach ($phase in 'red', 'kept', 'green') {
  python -I $window cut "$Work\receiver.wav" "$Work\receiver.stamps" (Join-Path $Work "scream-$phase.window") (Join-Path $Work "receiver-$phase") |
    Tee-Object -FilePath "$Work/result.txt" -Append
}
$redOut = python -I $heard check --label red --receiver "$Work\receiver-red.wav" "$Work\receiver-red.stamps" --sink "$red.wav" "$red.stamps" 2>&1
$redOk = $LASTEXITCODE -eq 0
$redOut | Tee-Object -FilePath "$Work/result.txt" -Append
Check 'VC51 red' $redOk 'the click train heard on the PC and on the receiver: heard twice'
$redAt = [regex]::Match(($redOut -join "`n"), 'offset: receiver (-?\d+) ms').Groups[1].Value
# Record only: whether the player opened before the install followed the default to the cable.
$keptOut = python -I $heard check --label kept --receiver "$Work\receiver-kept.wav" "$Work\receiver-kept.stamps" --sink "$kept.wav" "$kept.stamps" --absent-ms $redAt 2>&1
$keptOut | Tee-Object -FilePath "$Work/result.txt" -Append
$followed = if ($keptOut -match 'early: PASS') { 'yes' } elseif ($keptOut -match 'early: FAIL') { 'no, it kept the device it opened on' } else { 'not measured' }
Note "test player: Media.SoundPlayer (waveOut, MME); followed the default move: $followed"
"Test player: Media.SoundPlayer (waveOut, MME); followed the default move: $followed" | Out-File -Append -Encoding utf8 $summary
# 50 ms is S50's dead band, how far apart two outputs may sit before a correction.
$greenOut = python -I $heard check --label green --max-offset-ms 50 --receiver "$Work\receiver-green.wav" "$Work\receiver-green.stamps" --sink "$green.wav" "$green.stamps" --absent-ms $redAt 2>&1
$greenOnce = $LASTEXITCODE -eq 0
$greenOut | Tee-Object -FilePath "$Work/result.txt" -Append
Check 'VC52 green' $greenOnce "with VB-CABLE added by the $leg leg and the player restarted: the PC holds the train once, within 50 ms of the receiver and nothing $redAt ms early, and the receiver holds it once"
"Leg that produced the result: **$leg**$(if ($why) { " ($why)" })" | Out-File -Append -Encoding utf8 $summary

if ($script:failed) { throw "$($script:failed.Count) check(s) failed:`n$($script:failed -join "`n")" }
