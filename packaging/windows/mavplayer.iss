; Inno Setup script for the MavPlayer installer.
; Built by scripts/package-windows.ps1, which passes /DAppVersion and /DSourceDir.

#ifndef AppVersion
  #define AppVersion "0.3.3"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\MavPlayer"
#endif

[Setup]
AppId={{6F2B8C1E-4D3A-4B7E-9A51-8C2D7E0F4A13}
AppName=MavPlayer
AppVersion={#AppVersion}
AppPublisher=MavPlayer contributors
AppPublisherURL=https://github.com/Drogobo/mavplayer
AppSupportURL=https://github.com/Drogobo/mavplayer/issues
DefaultDirName={autopf}\MavPlayer
DefaultGroupName=MavPlayer
DisableProgramGroupPage=yes
LicenseFile=..\..\LICENSE
OutputDir=..\..\dist
OutputBaseFilename=MavPlayer-{#AppVersion}-windows-x64-setup
SetupIconFile=mavplayer.ico
UninstallDisplayIcon={app}\mavplayer.exe
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequiredOverridesAllowed=dialog
ChangesAssociations=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "associate"; Description: "Add MavPlayer to ""Open with"" for video and audio files"; GroupDescription: "File types:"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\MavPlayer"; Filename: "{app}\mavplayer.exe"
Name: "{autodesktop}\MavPlayer"; Filename: "{app}\mavplayer.exe"; Tasks: desktopicon

[Registry]
; Registered application + "Open with" entries (does not steal existing defaults).
Root: HKA; Subkey: "Software\Classes\Applications\mavplayer.exe"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "MavPlayer"; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\Applications\mavplayer.exe\shell\open\command"; ValueType: string; ValueData: """{app}\mavplayer.exe"" ""%1"""; Flags: uninsdeletekey
Root: HKA; Subkey: "Software\Classes\MavPlayer.Media"; ValueType: string; ValueData: "Media file"; Flags: uninsdeletekey; Tasks: associate
Root: HKA; Subkey: "Software\Classes\MavPlayer.Media\DefaultIcon"; ValueType: string; ValueData: "{app}\mavplayer.exe,0"; Tasks: associate
Root: HKA; Subkey: "Software\Classes\MavPlayer.Media\shell\open\command"; ValueType: string; ValueData: """{app}\mavplayer.exe"" ""%1"""; Tasks: associate
#define Assoc(str ext) 'Root: HKA; Subkey: "Software\Classes\' + ext + '\OpenWithProgids"; ValueType: string; ValueName: "MavPlayer.Media"; ValueData: ""; Flags: uninsdeletevalue; Tasks: associate'
#emit Assoc(".mkv")
#emit Assoc(".mp4")
#emit Assoc(".m4v")
#emit Assoc(".mov")
#emit Assoc(".avi")
#emit Assoc(".webm")
#emit Assoc(".wmv")
#emit Assoc(".flv")
#emit Assoc(".ts")
#emit Assoc(".m2ts")
#emit Assoc(".mpg")
#emit Assoc(".mpeg")
#emit Assoc(".vob")
#emit Assoc(".ogv")
#emit Assoc(".3gp")
#emit Assoc(".mp3")
#emit Assoc(".flac")
#emit Assoc(".ogg")
#emit Assoc(".opus")
#emit Assoc(".m4a")
#emit Assoc(".aac")
#emit Assoc(".wav")
#emit Assoc(".wma")
#emit Assoc(".ac3")
#emit Assoc(".m3u")
#emit Assoc(".m3u8")
#emit Assoc(".pls")

[Run]
Filename: "{app}\mavplayer.exe"; Description: "{cm:LaunchProgram,MavPlayer}"; Flags: nowait postinstall skipifsilent
