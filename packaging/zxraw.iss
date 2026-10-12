#define AppVersion "0.1.0"
#define PayloadDir "..\dist\ZXraw-Explorer-" + AppVersion + "-x64"

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

[Tasks]
Name: "contextmenu"; Description: "Add &Open in ZXraw Explorer to folder and drive menus"; Flags: unchecked
Name: "filetypes"; Description: "Offer ZXraw Explorer as the &default app for photos and RAW files (confirm in Settings)"; Flags: unchecked

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Registry]
Root: HKCU; Subkey: "Software\Classes\Directory\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Drive\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey
Root: HKCU; Subkey: "Software\Classes\Directory\Background\shell\ZXrawExplorer"; Flags: uninsdeletekey dontcreatekey

[Icons]
Name: "{autoprograms}\ZXraw Explorer"; Filename: "{app}\zxraw.exe"

[Run]
Filename: "{app}\zxraw.exe"; Parameters: "--register-context-menu"; Description: "Register Explorer context menu"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: contextmenu
Filename: "{app}\zxraw.exe"; Parameters: "--register-file-types"; Description: "Register photo file types"; Flags: runhidden postinstall skipifsilent waituntilterminated; Tasks: filetypes

[UninstallRun]
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-context-menu"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveZXrawExplorerContextMenu"
Filename: "{app}\zxraw.exe"; Parameters: "--unregister-file-types"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveZXrawExplorerFileTypes"
