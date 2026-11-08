; AstralGuard setup (Inno Setup 6). The release workflow builds it:
;   iscc /DAppVersion=2.0.0 /DSourceExe=<path to astralguard.exe> /DRepoRoot=<repo> /O<out> astralguard.iss
;
; Installs astralguard.exe to Program Files and, unless unticked, registers the logon task
; that starts it elevated (`astralguard register`). Settings and logs live in
; %ProgramData%\AstralGuard and survive uninstall and upgrades.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef RepoRoot
  #define RepoRoot "..\..\.."
#endif
#ifndef SourceExe
  #define SourceExe RepoRoot + "\rust\target\release\astralguard.exe"
#endif

[Setup]
AppId={{0550B77E-981D-43A3-8319-BC2F50A9C887}
AppName=AstralGuard
AppVersion={#AppVersion}
AppVerName=AstralGuard {#AppVersion}
AppPublisher=Szabolcs Gergely
AppPublisherURL=https://github.com/SzabolcsGergely/AstralGuard
AppSupportURL=https://github.com/SzabolcsGergely/AstralGuard/issues
AppUpdatesURL=https://github.com/SzabolcsGergely/AstralGuard/releases
DefaultDirName={autopf}\AstralGuard
DefaultGroupName=AstralGuard
DisableProgramGroupPage=yes
LicenseFile={#RepoRoot}\LICENSE
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=AstralGuard-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\astralguard.exe
UninstallDisplayName=AstralGuard
VersionInfoVersion={#AppVersion}
VersionInfoProductName=AstralGuard
VersionInfoDescription=AstralGuard setup
; The guard is stopped in PrepareToInstall; nothing else should hold the files.
CloseApplications=no

[Tasks]
Name: "autostart"; Description: "Start AstralGuard when I sign in (recommended: the guard only protects while it runs)"
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#RepoRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
Name: "{commonappdata}\AstralGuard"

[Icons]
Name: "{autoprograms}\AstralGuard"; Filename: "{app}\astralguard.exe"; Comment: "Per-pin 12V-2x6 current guard"
Name: "{autoprograms}\AstralGuard settings and logs"; Filename: "{commonappdata}\AstralGuard"
Name: "{autodesktop}\AstralGuard"; Filename: "{app}\astralguard.exe"; Tasks: desktopicon

[Run]
; The logon task (elevated, no time limit) or, if unticked, no task at all.
Filename: "{app}\astralguard.exe"; Parameters: "register"; Tasks: autostart; Flags: runhidden waituntilterminated; StatusMsg: "Registering the logon task..."
Filename: "{app}\astralguard.exe"; Parameters: "unregister"; Tasks: not autostart; Flags: runhidden waituntilterminated
; Start it now: through the task when there is one, so it runs exactly as it will at sign-in.
Filename: "{sys}\schtasks.exe"; Parameters: "/run /tn AstralGuard"; Tasks: autostart; Flags: postinstall runhidden; Description: "Start AstralGuard now"
Filename: "{app}\astralguard.exe"; Tasks: not autostart; Flags: postinstall nowait; Description: "Start AstralGuard now"

[UninstallRun]
Filename: "{app}\astralguard.exe"; Parameters: "unregister"; Flags: runhidden waituntilterminated; RunOnceId: "Unregister"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im astralguard.exe"; Flags: runhidden waituntilterminated; RunOnceId: "StopGuard"

[Code]
procedure StopGuard();
var
  Code: Integer;
begin
  { The running guard locks astralguard.exe. Ending the task ends the watchdog, whose job
    object takes the service and the dashboard with it; taskkill covers a copy started by
    hand. }
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn AstralGuard', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im astralguard.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopGuard();
  Result := '';
end;
