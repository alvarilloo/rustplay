; Classic installer for RustPlay. Build it with packaging/build-installer.ps1.
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define Root "..\.."

[Setup]
AppId={{6F1D2C3B-9E4A-4C1E-8B7A-2D5F3A9C1E44}
AppName=RustPlay
AppVersion={#AppVersion}
AppVerName=RustPlay {#AppVersion}
AppPublisher=RustPlay
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\RustPlay
DefaultGroupName=RustPlay
DisableProgramGroupPage=yes
LicenseFile={#Root}\LICENSE
OutputDir={#Root}\target\installer
OutputBaseFilename=RustPlay-Setup-{#AppVersion}
SetupIconFile={#Root}\crates\gui\assets\rustplay.ico
UninstallDisplayIcon={app}\RustPlay.exe
UninstallDisplayName=RustPlay
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
; Administrator rights are needed for the firewall rules.
PrivilegesRequired=admin
UsedUserAreasWarning=no
; Restart Manager closes a running RustPlay before files are replaced.
CloseApplications=yes
RestartApplications=no

[Languages]
; English first: it is used when the Windows language has no translation.
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"

[CustomMessages]
english.AutoStart=Start RustPlay with Windows (minimized, with the receiver on)
spanish.AutoStart=Iniciar RustPlay con Windows (minimizado y con el receptor activado)
english.StartupGroup=Startup:
spanish.StartupGroup=Inicio:
english.Firewall=Allowing RustPlay in Windows Firewall…
spanish.Firewall=Permitiendo RustPlay en el Firewall de Windows…
english.RemoveData=Do you also want to delete RustPlay's settings and the paired devices?
spanish.RemoveData=¿Quieres borrar también los ajustes de RustPlay y los dispositivos emparejados?

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "autostart"; Description: "{cm:AutoStart}"; GroupDescription: "{cm:StartupGroup}"; Flags: unchecked

[Files]
Source: "{#Root}\target\release\RustPlay.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\target\release\rustplay-cli.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Root}\NOTICE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Same AppUserModelID as crates/gui/src/notify.rs, so the taskbar and notifications match the shortcuts.
Name: "{autoprograms}\RustPlay"; Filename: "{app}\RustPlay.exe"; AppUserModelID: "RustPlay.App"
Name: "{autodesktop}\RustPlay"; Filename: "{app}\RustPlay.exe"; AppUserModelID: "RustPlay.App"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "RustPlay"; \
  ValueData: """{app}\RustPlay.exe"" --background"; Tasks: autostart; Flags: uninsdeletevalue
; RustPlay writes this key at start-up so Windows can show its notifications.
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\RustPlay.App"; Flags: uninsdeletekey dontcreatekey

[Run]
; Inbound rules for the receiver (TCP 7000 plus dynamic TCP/UDP ports) on private and domain networks.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""RustPlay"""; Flags: runhidden; StatusMsg: "{cm:Firewall}"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""RustPlay"" dir=in action=allow program=""{app}\RustPlay.exe"" enable=yes profile=private,domain"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""RustPlay"" dir=in action=allow program=""{app}\rustplay-cli.exe"" enable=yes profile=private,domain"; Flags: runhidden
Filename: "{app}\RustPlay.exe"; Description: "{cm:LaunchProgram,RustPlay}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/IM RustPlay.exe /F"; Flags: runhidden; RunOnceId: "StopRustPlay"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""RustPlay"""; Flags: runhidden; RunOnceId: "RemoveFirewallRules"

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    // The app may have registered itself from its own settings page.
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', 'RustPlay');
    if not UninstallSilent and
       (MsgBox(CustomMessage('RemoveData'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
      DelTree(ExpandConstant('{userappdata}\RustPlay'), True, True, True);
  end;
end;
