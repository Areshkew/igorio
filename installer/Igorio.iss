[Setup]
AppId={{DCA49C22-318F-49EF-87AA-C2684C01DEB1}
AppName=Igorio
AppVersion={#IgorioVersion}
DefaultDirName={autopf}\Igorio
DisableDirPage=yes
UsePreviousAppDir=no
DisableProgramGroupPage=yes
PrivilegesRequired=admin
MinVersion=10.0
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=Igorio-Setup-{#IgorioVersion}
SetupIconFile=..\assets\igorio.ico
UninstallDisplayIcon={app}\igorio.exe
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Tasks]
Name: "autostart"; Description: "Start Igorio when I sign in"; Flags: unchecked

[Files]
Source: "{#IgorioExe}"; DestDir: "{app}"; Flags: ignoreversion; BeforeInstall: CloseBeforeUpgrade
Source: "{#FallbackExe}"; Flags: dontcopy
Source: "install-support.ps1"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\reference\fairy-cursor\LICENSE"; DestDir: "{app}"; DestName: "LICENSE-fairy-cursor.txt"
Source: "..\assets\fonts\OFL.txt"; DestDir: "{app}"; DestName: "OFL-Barlow.txt"

[Icons]
Name: "{commonprograms}\Igorio"; Filename: "{app}\igorio.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\igorio.exe"; Description: "Launch Igorio"; WorkingDir: "{app}"; Flags: postinstall shellexec nowait skipifsilent runasoriginaluser unchecked

[Code]
const
  CloseMessage = 'Quit Igorio from its notification area menu, then try again.';

function RunSupport(const Action: String): Boolean;
var
  Code: Integer;
  Args: String;
begin
  Args := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' +
    ExpandConstant('{app}\install-support.ps1') + '" -Action ' + Action;
  try
    Result := ExecAndLogOutput(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      Args, ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, Code, nil) and (Code = 0);
    if not Result then
      Log(Format('Igorio install support (%s) failed with code %d.', [Action, Code]));
  except
    Log(GetExceptionMessage);
    Result := False;
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if CompareText(ExpandConstant('{app}'), ExpandConstant('{autopf}\Igorio')) <> 0 then
    Result := 'Igorio must be installed in ' + ExpandConstant('{autopf}\Igorio') + ' for UI access.';
end;

function CloseInstalledIgorio: Boolean;
var
  Exe: String;
  Stream: TFileStream;
begin
  Exe := ExpandConstant('{app}\igorio.exe');
  Result := not FileExists(Exe);
  if Result then
    Exit;

  if FileExists(ExpandConstant('{app}\install-support.ps1')) then
    RunSupport('Close');

  { Also works if PowerShell is unavailable. Opening an in-use executable for write fails. }
  try
    Stream := TFileStream.Create(Exe, fmOpenReadWrite or fmShareDenyNone);
    Stream.Free;
    Result := True;
  except
    Log(GetExceptionMessage);
  end;
end;

procedure CloseBeforeUpgrade;
begin
  { Restart Manager already had a chance to close the installed executable. }
  if not CloseInstalledIgorio then
    RaiseException(CloseMessage);
end;

procedure CurStepChanged(Step: TSetupStep);
var
  Args, Exe: String;
  Code: Integer;
begin
  if Step <> ssPostInstall then
    Exit;

  Exe := ExpandConstant('{app}\igorio.exe');
  if not RunSupport('Sign') then
  begin
    ExtractTemporaryFile('igorio-no-ui-access.exe');
    if not CopyFile(ExpandConstant('{tmp}\igorio-no-ui-access.exe'), Exe, False) then
      RaiseException('Igorio could not install the fallback executable. See the Setup log.');
    SuppressibleMsgBox('Igorio could not be signed. It was installed without access to administrator windows. See the Setup log for details.', mbError, MB_OK, IDOK);
  end;

  if WizardIsTaskSelected('autostart') then
  begin
    { Use the HKCU of the user before UAC, with the same quoted path as Settings. }
    Args := 'add HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v Igorio /t REG_SZ /d "\"' + Exe + '\"" /f';
    if not ExecAsOriginalUser(ExpandConstant('{sys}\reg.exe'), Args, '', SW_HIDE,
      ewWaitUntilTerminated, Code) or (Code <> 0) then
      SuppressibleMsgBox('Igorio could not enable launch at sign-in. You can try again from Settings.', mbError, MB_OK, IDOK);
  end;
end;

function InitializeUninstall: Boolean;
begin
  Result := CloseInstalledIgorio;
  if not Result then
    SuppressibleMsgBox(CloseMessage, mbError, MB_OK, IDOK);
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
begin
  if (Step = usUninstall) and not RunSupport('Uninstall') then
    SuppressibleMsgBox('Igorio could not remove its signing certificates or launch-at-sign-in value. See the uninstall log.', mbError, MB_OK, IDOK);
end;
