#define AppVersion "0.11.3"
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
Name: "archivetypes"; Description: "Register ZXraw Explorer for archive types (.zip, .7z, .rar, .tar, .gz and more)"; Flags: unchecked
Name: "archivemenu"; Description: "Add the ZXraw menu (extract, add to archive) to Explorer's right-click menu"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Registry]
; Removed only by the uninstaller: an upgrade keeps the user's context menu.
Root: HKCU; Subkey: "Software\Classes\Directory\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Drive\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
; The ZXraw menu's file and folder entries and its command classes, in case the uninstall
; switch above could not run. Per-extension entries are removed by --unregister-archive-menu.
Root: HKCU; Subkey: "Software\Classes\*\shell\ZXrawExplorer.Menu"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\ZXrawExplorer.Menu"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{AB314DBF-17E2-4FA6-87C2-7C905D9E48B9}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{FBFABA5C-357F-438E-8F19-238FE96632A4}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{2B87099A-A007-428D-8635-B4C0591DFBAB}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{2890F3EA-705E-445E-8E2D-07C2B4D14EBA}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{066E9532-19F2-45BA-A429-196D0B24F2FC}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{C452E1E2-0C6C-4DE3-A91A-A597D5D00FEA}"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\CLSID\{{38CE9404-C93C-4E5D-BCB9-DFB508C222FA}"; Flags: uninsdeletekey dontcreatekey


[Icons]
Name: "{autoprograms}\ZXraw Explorer"; Filename: "{app}\zxraw.exe"

[Run]
Filename: "{app}\zxraw.exe"; Parameters: "--register-context-menu"; Description: "Register Explorer context menu"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: contextmenu
Filename: "{app}\zxraw.exe"; Parameters: "--register-file-types"; Description: "Register photo file types"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: filetypes
Filename: "{app}\zxraw.exe"; Parameters: "--register-archive-types"; Description: "Register archive file types"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: archivetypes
Filename: "{app}\zxraw.exe"; Parameters: "--register-archive-menu"; Description: "Add the ZXraw menu to Explorer"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: archivemenu
; In-app updates pass /RELAUNCH: start the new version once the silent upgrade is done.
Filename: "{app}\zxraw.exe"; Flags: nowait; Check: RelaunchRequested

[UninstallRun]
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-default-folder"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreZXrawExplorerFolderDefault"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-context-menu"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveZXrawExplorerContextMenu"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-file-types"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreZXrawExplorerPhotoDefaults"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-archive-menu"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveZXrawExplorerArchiveMenu"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-archive-types"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreZXrawExplorerArchiveDefaults"

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
