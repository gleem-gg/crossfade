; Inno Setup script for Gleem Crossfade on Windows.
; Built by build-installer.ps1, which stages the files and passes
; /DAppVersion and /DStage.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Stage
  #define Stage "build\stage"
#endif

[Setup]
; Never change the AppId: it is how upgrades and the uninstaller find an
; existing installation.
AppId={{B06D5852-5DAD-4BC6-9B4A-BFF535431A7F}
AppName=Gleem Crossfade
AppVersion={#AppVersion}
AppVerName=Gleem Crossfade {#AppVersion}
AppPublisher=Anikeen UG (haftungsbeschränkt) & Co. KG
AppPublisherURL=https://gleem.gg/crossfade
AppSupportURL=https://github.com/gleem-gg/crossfade
AppCopyright=© 2026 Anikeen UG (haftungsbeschränkt) & Co. KG
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\Gleem Crossfade
DisableProgramGroupPage=yes
; Installing a driver needs administrator rights.
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 2004: per-process loopback/policy APIs the engine relies on.
MinVersion=10.0.19041
OutputBaseFilename=GleemCrossfade-{#AppVersion}-setup
SetupIconFile=gleem-crossfade.ico
UninstallDisplayIcon={app}\bin\gleem-crossfade.exe
UninstallDisplayName=Gleem Crossfade
LicenseFile={#Stage}\LICENSE.txt
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; A running Crossfade is quit through its own `--quit` (see [Code]), which
; also releases the per-app routes; Restart Manager would only kill it.
CloseApplications=no
SetupLogging=yes

[Files]
Source: "{#Stage}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\Gleem Crossfade"; Filename: "{app}\bin\gleem-crossfade.exe"

[Registry]
; The app's own "Start at Login" entry (per user); removed on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Gleem Crossfade"; Flags: uninsdeletevalue

[Run]
Filename: "{app}\bin\gleem-crossfade.exe"; Description: "Launch Gleem Crossfade"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; Runs before the files are removed: driver device, driver packages and the
; endpoint naming service (with its copy in Program Files).
Filename: "{app}\driver\crossfade-driver-setup.exe"; Parameters: "uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveCrossfadeDriver"

[Code]
const
  DriverOk = 0;
  DriverRebootNeeded = 3010;

var
  DriverNeedsRestart: Boolean;

// Ask a running Crossfade to quit (it saves, unloads its devices and
// clears per-app routes), wait for it, and only force it as a last resort.
procedure QuitRunningCrossfade(const Exe: String);
var
  ResultCode: Integer;
begin
  if FileExists(Exe) then
    Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -Command "Wait-Process -Name gleem-crossfade -Timeout 10 -ErrorAction SilentlyContinue"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM gleem-crossfade.exe', '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  // Upgrades: the running app holds its files and the driver open.
  QuitRunningCrossfade(ExpandConstant('{app}\bin\gleem-crossfade.exe'));
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  QuitRunningCrossfade(ExpandConstant('{app}\bin\gleem-crossfade.exe'));
  Result := True;
end;

procedure InstallDriver();
var
  ResultCode: Integer;
  Log: String;
  Output: AnsiString;
begin
  WizardForm.StatusLabel.Caption := 'Installing the Crossfade audio driver...';
  Log := ExpandConstant('{tmp}\driver-setup.log');
  Exec(ExpandConstant('{cmd}'),
    '/C ""' + ExpandConstant('{app}\driver\crossfade-driver-setup.exe') + '" install > "' + Log + '" 2>&1"',
    '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  if ResultCode = DriverRebootNeeded then
    DriverNeedsRestart := True
  else if ResultCode <> DriverOk then
  begin
    LoadStringFromFile(Log, Output);
    SuppressibleMsgBox(
      'Gleem Crossfade was installed, but its audio driver could not be set up, ' +
      'so the virtual devices are not available yet.' + #13#10#13#10 +
      'Development builds of the driver only load in Windows test-signing mode ' +
      '(see driver\README.md in the source).' + #13#10#13#10 +
      'Details:' + #13#10 + String(Output),
      mbError, MB_OK, IDOK);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    InstallDriver();
end;

function NeedRestart(): Boolean;
begin
  Result := DriverNeedsRestart;
end;
