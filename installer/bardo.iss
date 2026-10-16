; Inno Setup script for bardo-setup.exe. Installs for the current user only, so it needs no admin rights.
;
;   iscc /DVersion=0.1.0 installer\bardo.iss
;
; Expects target\release\bardo.exe and writes target\installer\bardo-setup.exe.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
; Identifies bardo to Windows across versions: never change it.
AppId={{1860814F-1E34-4604-A07B-CD8CED0F364E}
AppName=bardo
AppVersion={#Version}
AppVerName=bardo {#Version}
AppPublisher=bardo
AppPublisherURL=https://github.com/luchoov/bardo
AppSupportURL=https://github.com/luchoov/bardo/issues
AppUpdatesURL=https://github.com/luchoov/bardo/releases
DefaultDirName={localappdata}\Programs\bardo
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\target\installer
OutputBaseFilename=bardo-setup
SetupIconFile=..\app\assets\bardo.ico
UninstallDisplayIcon={app}\bardo.exe
UninstallDisplayName=bardo
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Closes a running bardo before replacing it.
CloseApplications=yes

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\bardo.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\bardo"; Filename: "{app}\bardo.exe"
Name: "{userdesktop}\bardo"; Filename: "{app}\bardo.exe"; Tasks: desktopicon

[Run]
; Also when silent: that is how bardo updates itself, and it should start again afterwards.
Filename: "{app}\bardo.exe"; Description: "Open bardo"; Flags: nowait postinstall

[Code]
// bardo adds this itself when "Start with Windows" is on; it goes with bardo.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'bardo');
end;
