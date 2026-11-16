; Inno Setup script for ZP. Built by scripts/package-windows.ps1, which passes
; /DVersion=<version> /DSource=<path to zp.exe> /DOutput=<dist dir>.
#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppId={{6F0F7C5B-2B6B-4C47-9B5E-5A1E2C7D0A01}
AppName=ZP
AppVersion={#Version}
AppPublisher=ZP
DefaultDirName={autopf}\ZP
DefaultGroupName=ZP
OutputDir={#Output}
OutputBaseFilename=ZP-{#Version}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
SetupIconFile=zp.ico
UninstallDisplayIcon={app}\zp.exe
WizardStyle=modern

[Files]
Source: "{#Source}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\ZP"; Filename: "{app}\zp.exe"
Name: "{autodesktop}\ZP"; Filename: "{app}\zp.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\zp.exe"; Description: "Launch ZP"; Flags: nowait postinstall skipifsilent
