; Windows installer with explicit current-user support. WinGet passes
; /CURRENTUSER so Setup stays in non-administrative install mode, while the
; command-line override lets Setup use administrative mode when requested.
; [Run] entries omit the postinstall flag so they still
; execute under winget's silent Inno switches.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\..\target\release\ullage.exe"
#endif
#ifndef LicenseFile
  #define LicenseFile "..\..\LICENSE"
#endif
#ifndef ReadmeFile
  #define ReadmeFile "..\..\README.md"
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
AppId={{C0A1B8E4-5D27-4F91-9C3A-7E6B2D4F8A15}
AppName=Ullage
AppVersion={#AppVersion}
AppPublisher=dualface
AppPublisherURL=https://github.com/dualface/ullage-cli
AppSupportURL=https://github.com/dualface/ullage-cli/issues
DefaultDirName={localappdata}\Ullage
DisableProgramGroupPage=yes
LicenseFile={#LicenseFile}
OutputDir={#OutputDir}
OutputBaseFilename=ullage-x86_64-pc-windows-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\ullage.exe
ChangesEnvironment=yes
MinVersion=10.0
CloseApplications=yes
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "ullage.exe"; Flags: ignoreversion
Source: "{#LicenseFile}"; DestDir: "{app}"; DestName: "LICENSE"; Flags: ignoreversion
Source: "{#ReadmeFile}"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion

[Run]
; Stop first so an upgrade can replace ullage.exe and bootstrap the new path.
Filename: "{app}\ullage.exe"; Parameters: "daemon stop"; Flags: runhidden waituntilterminated
Filename: "{app}\ullage.exe"; Parameters: "daemon install"; Flags: runhidden waituntilterminated
Filename: "{app}\ullage.exe"; Parameters: "daemon start"; Flags: runhidden waituntilterminated

[UninstallRun]
Filename: "{app}\ullage.exe"; Parameters: "daemon stop"; Flags: runhidden waituntilterminated; RunOnceId: "StopDaemon"
Filename: "{app}\ullage.exe"; Parameters: "daemon uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "UninstallDaemon"

[Code]
function NeedsAddPath(const Path: string): Boolean;
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
  begin
    Result := True;
    Exit;
  end;
  Result := Pos(';' + Uppercase(Path) + ';', ';' + Uppercase(Paths) + ';') = 0;
end;

procedure EnvAddPath(const Path: string);
var
  Paths: string;
begin
  if not NeedsAddPath(Path) then
    Exit;
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
    Paths := '';
  if Paths = '' then
    Paths := Path
  else
    Paths := Paths + ';' + Path;
  RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths);
end;

procedure EnvRemovePath(const Path: string);
var
  Paths: string;
  Needle: string;
  PathsUpper: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
    Exit;
  Needle := ';' + Uppercase(Path) + ';';
  PathsUpper := ';' + Uppercase(Paths) + ';';
  P := Pos(Needle, PathsUpper);
  if P = 0 then
    Exit;
  Delete(Paths, P, Length(Path) + 1);
  if (Length(Paths) > 0) and (Paths[1] = ';') then
    Delete(Paths, 1, 1);
  if (Length(Paths) > 0) and (Paths[Length(Paths)] = ';') then
    Delete(Paths, Length(Paths), 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  NeedsRestart := False;
  if FileExists(ExpandConstant('{app}\ullage.exe')) then
    Exec(ExpandConstant('{app}\ullage.exe'), 'daemon stop', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;
