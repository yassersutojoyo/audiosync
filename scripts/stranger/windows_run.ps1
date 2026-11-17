# A stranger's install on Windows, run by .github/workflows/stranger.yml:
# the default output endpoint as found, a silent install of AudioSync-Setup.exe,
# a launch with AUDIOSYNC_DEBUG=1 and a screenshot, the uninstall while the app
# runs, and the default output checked against the one found (the restore of
# S469 when it moved). Sound is measured only when the audio service runs
# and an active output endpoint exists; otherwise the run says "not measured".
#
#   pwsh scripts/stranger/windows_run.ps1 -Setup <AudioSync-Setup.exe> -Work <folder>
param(
  [Parameter(Mandatory)] [string] $Setup,
  [Parameter(Mandatory)] [string] $Work
)
$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path $Work | Out-Null

# IMMDeviceEnumerator and IMMDevice as the Windows SDK's mmdeviceapi.h declares them.
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
}

[ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
  [PreserveSig] int Activate(ref Guid iid, int context, IntPtr parameters, [MarshalAs(UnmanagedType.IUnknown)] out object face);
  [PreserveSig] int OpenPropertyStore(int access, out IntPtr store);
  [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
}

public static class Endpoints {
  const int Render = 0, Active = 1;

  static IMMDeviceEnumerator Enumerator() {
    return (IMMDeviceEnumerator)new MMDeviceEnumeratorClass();
  }

  // role 0 is eConsole, 1 eMultimedia.
  public static string Default(int role) {
    try {
      IMMDevice device;
      int hr = Enumerator().GetDefaultAudioEndpoint(Render, role, out device);
      if (hr != 0) return string.Format("none (0x{0:X8})", hr);
      string id;
      hr = device.GetId(out id);
      return hr == 0 ? id : string.Format("unreadable (0x{0:X8})", hr);
    } catch (Exception e) {
      return "unreadable (" + e.GetType().Name + ")";
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
}
'@

function Get-Defaults {
  [ordered]@{ console = [Endpoints]::Default(0); multimedia = [Endpoints]::Default(1) }
}

$os = Get-CimInstance Win32_OperatingSystem
"os: $($os.Caption) build $($os.BuildNumber)" | Tee-Object -FilePath "$Work/env.txt"

# Windows Server ships the audio service disabled; a stranger's PC has it on.
foreach ($name in 'AudioEndpointBuilder', 'Audiosrv') {
  try {
    Set-Service -Name $name -StartupType Automatic
    Start-Service -Name $name
  } catch {
    "audio service ${name}: could not start ($($_.Exception.Message))"
  }
}
$service = (Get-Service Audiosrv).Status
$endpoints = [Endpoints]::ActiveCount()
"audio service: $service, active output endpoints: $endpoints" | Tee-Object -FilePath "$Work/env.txt" -Append

$before = Get-Defaults
"default output before install: console=$($before.console) multimedia=$($before.multimedia)"

$status = 0
$installLog = Join-Path $Work 'install.log'
$install = Start-Process -FilePath $Setup -Wait -PassThru `
  -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/ALLUSERS', "/LOG=`"$installLog`""
$dir = Join-Path $env:ProgramFiles 'AudioSync'
$exe = Join-Path $dir 'audiosync.exe'
if ($install.ExitCode -ne 0 -or -not (Test-Path $exe)) {
  throw "install failed (exit $($install.ExitCode)); see $installLog"
}
"installed: $exe $((Get-Item $exe).VersionInfo.ProductVersion)"

$env:AUDIOSYNC_DEBUG = '1'
$app = Start-Process -FilePath $exe -WorkingDirectory $dir -PassThru
Start-Sleep -Seconds 20
Remove-Item Env:AUDIOSYNC_DEBUG

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
try {
  $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $shot = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
  [System.Drawing.Graphics]::FromImage($shot).CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
  $shot.Save((Join-Path $Work 'screen.png'), [System.Drawing.Imaging.ImageFormat]::Png)
  "screenshot: screen.png ($($bounds.Width)x$($bounds.Height))"
} catch {
  "screenshot: not taken ($($_.Exception.Message))"
}

if ($app.HasExited) {
  "FAIL: the app exited within 20 s (code $($app.ExitCode))"
  $status = 1
} else {
  'app: running after 20 s'
}
$during = Get-Defaults
"default output while running: console=$($during.console) multimedia=$($during.multimedia)"

$log = Join-Path $env:TEMP 'audiosync-debug.log'
if (Test-Path $log) {
  Copy-Item $log (Join-Path $Work 'audiosync-debug.log')
  if (Select-String -Path $log -Pattern 'panic' -Quiet) {
    'FAIL: the debug log has a panic'
    $status = 1
  }
  '--- error and warning lines in the debug log (information)'
  Select-String -Path $log -Pattern 'error|warning' | ForEach-Object { $_.Line }
} else {
  "FAIL: no debug log at $log"
  $status = 1
}

if ($endpoints -gt 0 -and $service -eq 'Running') {
  'sound: not measured (an endpoint is up, but this workflow has no recorder for it)'
} else {
  "sound: not measured (audio service $service, $endpoints active output endpoints)"
}

# The uninstaller closes the running app itself, as for a stranger who never quits it.
$uninstaller = Join-Path $dir 'unins000.exe'
Start-Process -FilePath $uninstaller -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
for ($i = 0; $i -lt 60 -and (Test-Path $exe); $i++) { Start-Sleep -Seconds 1 }
if (Test-Path $exe) {
  "FAIL: $exe is still there after the uninstall"
  $status = 1
} else {
  'uninstalled'
}
if (-not $app.HasExited) {
  'FAIL: the app still runs after the uninstall'
  Stop-Process -Id $app.Id -Force
  $status = 1
}

$after = Get-Defaults
"default output after uninstall: console=$($after.console) multimedia=$($after.multimedia)"
foreach ($role in 'console', 'multimedia') {
  if ($after[$role] -ne $before[$role]) {
    "FAIL: the $role default changed from $($before[$role]) to $($after[$role])"
    $status = 1
  }
}
if ($before.console -like 'none*') {
  'restore proof: not measured (no default output on this runner, so nothing could move)'
} else {
  'restore proof: the default output is as found'
}
exit $status
