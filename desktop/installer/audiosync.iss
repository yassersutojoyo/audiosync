; The Windows installer, built by scripts/build_installer.sh with Inno Setup 6.
; ISCC gets /DArch=x64 or /DArch=arm64 and /DSourceDir=<folder holding
; audiosync.exe and THIRD-PARTY-NOTICES.txt>, the same two files as the zip.

#ifndef Arch
  #error Pass /DArch=x64 or /DArch=arm64
#endif
#ifndef SourceDir
  #error Pass /DSourceDir=<folder with audiosync.exe>
#endif

; The choice preselected in the "Only for me" / "All users" dialog and used by
; a silent install without /CURRENTUSER or /ALLUSERS: lowest (only for me) or admin (all users).
#define DefaultInstallMode "admin"

#define AppVersion GetStringFileInfo(AddBackslash(SourceDir) + "audiosync.exe", "ProductVersion")
#if AppVersion == ""
  #error audiosync.exe has no ProductVersion
#endif

[Setup]
; Never change: Windows finds an earlier install, and its uninstall entry, by this id.
AppId={{85361868-5B14-4E92-80A1-E6C401069633}
AppName=AudioSync
AppVersion={#AppVersion}
AppPublisher=yassersutojoyo
AppPublisherURL=https://github.com/yassersutojoyo/audiosync
AppSupportURL=https://github.com/yassersutojoyo/audiosync/issues
AppUpdatesURL=https://github.com/yassersutojoyo/audiosync/releases
; "Only for me" needs no administrator: {autopf} is then %LOCALAPPDATA%\Programs,
; where scripts/install.ps1 installs. "All users" asks for administrator once and
; goes to Program Files, where each in-app update asks once more (docs/ENGINE_SPEC.md S457).
PrivilegesRequired={#DefaultInstallMode}
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\AudioSync
DisableDirPage=yes
DisableProgramGroupPage=yes
UninstallDisplayName=AudioSync
UninstallDisplayIcon={app}\audiosync.exe
SetupIconFile=..\res\audiosync.ico
#if Arch == "x64"
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=AudioSync-Setup
#elif Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
OutputBaseFilename=AudioSync-Setup-arm64
#else
  #error Arch must be x64 or arm64
#endif
WizardStyle=modern dynamic
; The app icon on the last page and in the corner of the others, on the app's
; window background (brand/tokens.json "background").
WizardImageFile=..\res\audiosync-256.png
WizardSmallImageFile=..\res\audiosync-256.png
WizardImageStretch=no
WizardImageBackColor=#F1F7F6
WizardImageBackColorDynamicDark=#1E1E1E
InfoBeforeFile=privacy.txt
Compression=lzma2
SolidCompression=yes
; A running copy holds audiosync.exe; install.ps1 stops it too. The app is
; started again only through the checkbox on the last page.
CloseApplications=force
RestartApplications=no

[Files]
Source: "{#SourceDir}\audiosync.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\AudioSync"; Filename: "{app}\audiosync.exe"; WorkingDir: "{app}"

[Run]
#if Arch == "x64"
; Adds VB-CABLE under Setup's own elevation, so no prompt of its own; a failure
; leaves AudioSync installed (docs/WINDOWS_SILENT_OUTPUT_SPEC.md VC25).
Filename: "{app}\audiosync.exe"; Parameters: "--add-vb-cable"; StatusMsg: "Adding VB-CABLE"; Flags: runhidden waituntilterminated; Check: AddsVbCable
#endif
; postinstall entries run as the user who started Setup, never elevated.
Filename: "{app}\audiosync.exe"; Description: "Open AudioSync"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Puts back the outputs a stream moved (S469, S471) while the files are still there (S474).
Filename: "{app}\audiosync.exe"; Parameters: "--restore-outputs"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreOutputs"
; The folder exists only while a cable the step above added does (VC53).
Filename: "{app}\VB-CABLE\VBCABLE_Setup_x64.exe"; Parameters: "-u -h"; Flags: runhidden waituntilterminated skipifdoesntexist; RunOnceId: "RemoveVbCable"

[UninstallDelete]
; What the app's updater leaves next to the files above (S301, S304).
Type: files; Name: "{app}\audiosync.exe.bak"
Type: files; Name: "{app}\audiosync.exe.new"
Type: files; Name: "{app}\THIRD-PARTY-NOTICES.txt.bak"
Type: files; Name: "{app}\THIRD-PARTY-NOTICES.txt.new"
; What the VB-CABLE step extracted (VC37).
Type: filesandordirs; Name: "{app}\VB-CABLE"

[Code]
#if Arch == "x64"
var
  VbCableKnown, VbCableThere, ReadyPageSaid: Boolean;

// A VB-CABLE device in any state, by its hardware ID under the ROOT
// enumerator, as the app finds it (VC9, VC21).
function HasVbCable: Boolean;
var
  Classes, Devices: TArrayOfString;
  I, J: Integer;
  Key, Ids: String;
begin
  if not VbCableKnown then
  begin
    VbCableKnown := True;
    if RegGetSubkeyNames(HKLM, 'SYSTEM\CurrentControlSet\Enum\ROOT', Classes) then
      for I := 0 to GetArrayLength(Classes) - 1 do
      begin
        Key := 'SYSTEM\CurrentControlSet\Enum\ROOT\' + Classes[I];
        if RegGetSubkeyNames(HKLM, Key, Devices) then
          for J := 0 to GetArrayLength(Devices) - 1 do
            if RegQueryMultiStringValue(HKLM, Key + '\' + Devices[J], 'HardwareID', Ids) and
              (Pos(#0 + 'vbaudiovacwdm' + #0, #0 + Lowercase(Ids) + #0) > 0) then
              VbCableThere := True;
      end;
  end;
  Result := VbCableThere;
end;

// For everyone is the install that is already elevated (VC25).
function AddsVbCable: Boolean;
begin
  Result := IsAdminInstallMode and not HasVbCable;
end;

// The Ready to install page names the step with VB-Audio's two sentences (VC22).
procedure CurPageChanged(CurPageID: Integer);
var
  Grown: Integer;
begin
  if (CurPageID = wpReady) and not ReadyPageSaid and AddsVbCable then
  begin
    ReadyPageSaid := True;
    WizardForm.ReadyLabel.Caption :=
      'It also adds VB-CABLE by VB-Audio, so you hear your PC''s sound once.' + #13#10#13#10 +
      'The origin of VB-CABLE : www.vb-cable.com.' + #13#10 +
      'VB-CABLE is a donationware, all participations are welcome.' + #13#10#13#10 +
      SetupMessage(msgReadyLabel2b);
    Grown := WizardForm.ReadyLabel.AdjustHeight;
    WizardForm.ReadyMemo.Top := WizardForm.ReadyMemo.Top + Grown;
    WizardForm.ReadyMemo.Height := WizardForm.ReadyMemo.Height - Grown;
  end;
end;
#endif

// The uninstaller has no Restart Manager step like CloseApplications, and a
// running copy holds audiosync.exe, so it is stopped here as install.ps1 does.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Exe: String;
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    Exe := ExpandConstant('{app}\audiosync.exe');
    StringChangeEx(Exe, '''', '''''', True);
    Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -NonInteractive -Command "Get-Process audiosync -ErrorAction SilentlyContinue | ' +
      'Where-Object { $_.Path -eq ''' + Exe + ''' } | ' +
      'ForEach-Object { $_ | Stop-Process -Force; $_ | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue }"',
      '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  end;
end;

[Messages]
SetupWindowTitle=Install %1
PrivilegesRequiredOverrideTitle=Who uses AudioSync?
PrivilegesRequiredOverrideInstruction=Install for everyone or only for you
PrivilegesRequiredOverrideText1=For everyone on this PC needs an administrator now, and once more for each update. Only for you needs neither.
PrivilegesRequiredOverrideText2=Only for you needs no administrator. For everyone on this PC needs one now, and once more for each update.
PrivilegesRequiredOverrideAllUsers=For &everyone on this PC
PrivilegesRequiredOverrideAllUsersRecommended=For &everyone on this PC
PrivilegesRequiredOverrideCurrentUser=Only for &me
WizardInfoBefore=Privacy
InfoBeforeLabel=What AudioSync sends and what it keeps on this PC.
InfoBeforeClickLabel=Select Next to continue.
WizardReady=Ready to install
ReadyLabel1=AudioSync is ready to install.
ReadyLabel2b=Select Install to continue.
FinishedHeadingLabel=AudioSync is installed
FinishedLabelNoIcons=You can open it now.
FinishedLabel=Find it in the Start menu, or open it now.
ClickFinish=Select Finish to close Setup.
