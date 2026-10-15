#define AppVersion "0.5.0"
#define PayloadDir "..\dist\ZXraw-Explorer-" + AppVersion + "-x64"
; Created by the running app (updater::APP_MUTEX in crates/forge-app/src/updater.rs).
#define AppMutexName "ZXrawExplorerRunning"

[Setup]
AppId={{752ECE11-5EAF-4D3E-94A0-F91B8804168B}
AppName=ZXraw Explorer
AppVersion={#AppVersion}
AppPublisher=ZXraw Explorer contributors
DefaultDirName={localappdata}\Programs\ZXraw Explorer
DefaultGroupName=ZXraw Explorer
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=ZXraw-Explorer-{#AppVersion}-Setup-x64
SetupIconFile=..\assets\zxraw.ico
UninstallDisplayIcon={app}\zxraw.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
; Setup and Uninstall refuse to run while the app is open. An update started from the app
; (/RELAUNCH) first waits for the app to finish closing; see AppMutexAfterClose.
AppMutex={code:AppMutexAfterClose}

[Tasks]
Name: "contextmenu"; Description: "Add &Open in ZXraw Explorer to folder and drive menus"; Flags: unchecked
Name: "filetypes"; Description: "Register ZXraw Explorer for photo and RAW file types"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Registry]
; Removed only by the uninstaller: an upgrade keeps the user's context menu.
Root: HKCU; Subkey: "Software\Classes\Directory\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Drive\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey

[Icons]
Name: "{autoprograms}\ZXraw Explorer"; Filename: "{app}\zxraw.exe"

[Run]
Filename: "{app}\zxraw.exe"; Parameters: "--register-context-menu"; Description: "Register Explorer context menu"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: contextmenu
Filename: "{app}\zxraw.exe"; Parameters: "--register-file-types"; Description: "Register photo file types"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: filetypes
; In-app updates pass /RELAUNCH: start the new version once the silent upgrade is done.
Filename: "{app}\zxraw.exe"; Flags: nowait; Check: RelaunchRequested

[UninstallRun]
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-default-folder"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreZXrawExplorerFolderDefault"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-context-menu"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveZXrawExplorerContextMenu"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-file-types"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreZXrawExplorerPhotoDefaults"

[Code]
function RelaunchRequested(): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), '/RELAUNCH') = 0 then
      Result := True;
end;

{ The app starts Setup and then closes itself, so Setup can see the app's mutex for a
  moment. When started that way, wait up to 30 seconds for it to go before the AppMutex
  check, which would otherwise cancel a silent install. }
function AppMutexAfterClose(Param: String): String;
var
  Waited: Integer;
begin
  Result := '{#AppMutexName}';
  if RelaunchRequested() then
  begin
    Waited := 0;
    while CheckForMutexes(Result) and (Waited < 30000) do
    begin
      Sleep(250);
      Waited := Waited + 250;
    end;
    Log(Format('Waited %d ms for ZXraw Explorer to close', [Waited]));
  end;
end;
