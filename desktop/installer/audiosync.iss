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
; goes to Program Files, where each in-app update asks once more (docs/ENGINE_SPEC.md S460).
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
; postinstall entries run as the user who started Setup, never elevated.
Filename: "{app}\audiosync.exe"; Description: "Open AudioSync"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What the app's updater leaves next to the files above (S301, S304).
Type: files; Name: "{app}\audiosync.exe.bak"
Type: files; Name: "{app}\audiosync.exe.new"
Type: files; Name: "{app}\THIRD-PARTY-NOTICES.txt.bak"
Type: files; Name: "{app}\THIRD-PARTY-NOTICES.txt.new"

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
