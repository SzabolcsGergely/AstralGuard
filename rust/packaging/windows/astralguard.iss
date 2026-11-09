; AstralGuard setup (Inno Setup 6). The release workflow builds it:
;   iscc /DAppVersion=2.0.0 /DSourceExe=<path to astralguard.exe> /DRepoRoot=<repo> /O<out> astralguard.iss
;
; Setup first asks who to install for (Inno Setup's install mode dialog):
;   All users  (administrator): Program Files, Start menu for everyone, and a logon task
;              that starts the guard for whoever signs in (`astralguard register --all-users`).
;   Just me    : the user's own Programs folder and Start menu, and a logon task for this
;              user only (`astralguard register`, one UAC prompt: the guard needs
;              administrator rights to limit the GPU, and so does creating its task).
; Settings and logs live in %ProgramData%\AstralGuard and survive uninstall and upgrades.
; An upgrade keeps the mode of the first install.

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
; "Install for all users" (default, needs administrator) or "Install for me only".
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
UsedUserAreasWarning=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=AstralGuard-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\astralguard.exe
SetupIconFile={#RepoRoot}\rust\assets\icon\astralguard.ico
UninstallDisplayName=AstralGuard
VersionInfoVersion={#AppVersion}
VersionInfoProductName=AstralGuard
VersionInfoDescription=AstralGuard setup
; The guard is stopped in PrepareToInstall; nothing else should hold the files.
CloseApplications=no

[Tasks]
Name: "autostart"; Description: "Start AstralGuard when any user signs in (recommended: the guard only protects while it runs)"; Check: IsAdminInstallMode
Name: "autostartme"; Description: "Start AstralGuard when I sign in (recommended: the guard only protects while it runs)"; Check: not IsAdminInstallMode
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#RepoRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Dirs]
Name: "{commonappdata}\AstralGuard"; Check: IsAdminInstallMode

[Icons]
Name: "{autoprograms}\AstralGuard"; Filename: "{app}\astralguard.exe"; Comment: "Per-pin 12V-2x6 current guard"
Name: "{autoprograms}\AstralGuard settings and logs"; Filename: "{commonappdata}\AstralGuard"
Name: "{autodesktop}\AstralGuard"; Filename: "{app}\astralguard.exe"; Tasks: desktopicon

[Run]
; All users: setup runs elevated, so everything runs directly.
Filename: "{app}\astralguard.exe"; Parameters: "register --all-users"; Tasks: autostart; Flags: runhidden waituntilterminated; StatusMsg: "Registering the logon task..."
Filename: "{app}\astralguard.exe"; Parameters: "unregister"; Check: IsAdminInstallMode and not WizardIsTaskSelected('autostart'); Flags: runhidden waituntilterminated
; Just me: creating an elevated logon task needs administrator rights (one UAC prompt).
Filename: "{app}\astralguard.exe"; Parameters: "register"; Tasks: autostartme; Verb: "runas"; Flags: shellexec runhidden waituntilterminated; StatusMsg: "Registering the logon task..."
; Start it now: through the task when there is one, so it runs exactly as it will at sign-in
; (this user may run their own task without elevating; setup is elevated for all users).
Filename: "{sys}\schtasks.exe"; Parameters: "/run /tn AstralGuard"; Tasks: autostart autostartme; Flags: postinstall runhidden; Description: "Start AstralGuard now"
Filename: "{app}\astralguard.exe"; Check: not WizardIsTaskSelected('autostart') and not WizardIsTaskSelected('autostartme'); Flags: postinstall nowait; Description: "Start AstralGuard now"

[UninstallRun]
Filename: "{app}\astralguard.exe"; Parameters: "unregister"; Check: IsAdminInstallMode; Flags: runhidden waituntilterminated; RunOnceId: "Unregister"
Filename: "{app}\astralguard.exe"; Parameters: "unregister"; Check: not IsAdminInstallMode; Verb: "runas"; Flags: shellexec runhidden waituntilterminated; RunOnceId: "UnregisterMe"
Filename: "{sys}\taskkill.exe"; Parameters: "/f /im astralguard.exe"; Flags: runhidden waituntilterminated; RunOnceId: "StopGuard"

[Code]
function GuardRunning(): Boolean;
var
  Code: Integer;
begin
  Result := Exec(ExpandConstant('{cmd}'), '/c tasklist /fi "imagename eq astralguard.exe" /nh | find /i "astralguard.exe"',
    '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

procedure StopGuard();
var
  Code: Integer;
begin
  { The running guard locks astralguard.exe. Ending the task ends the watchdog, whose job
    object takes the service and the dashboard with it; taskkill covers a copy started by
    hand, or an older install under one of the app's previous names. }
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn AstralGuard', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn PinGuard', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\schtasks.exe'), '/end /tn "12VHPWR Guard"', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im astralguard.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /im pinguard.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  { Just me: setup is not elevated and cannot end the elevated guard. One UAC prompt. }
  if (not IsAdminInstallMode) and GuardRunning() then
    ShellExec('runas', ExpandConstant('{sys}\taskkill.exe'), '/f /im astralguard.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopGuard();
  Result := '';
end;
