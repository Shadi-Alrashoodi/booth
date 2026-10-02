; The optional installer. The zip stays the main way to get Booth. This puts
; the same files in the user's own programs folder, adds a Start menu entry
; and an uninstaller, and never asks for administrator rights.
; tools\release.ps1 compiles it with Inno Setup 6.7.3 and passes the version
; and the folders:
;   ISCC /DAppVersion=0.1.0 /DSourceDir=<the unzipped release> /DOutputDir=<dist\0.1.0> tools\booth.iss

#ifndef AppVersion
  #error Build the installer through tools\release.ps1, which passes AppVersion, SourceDir and OutputDir.
#endif
#ifndef SourceDir
  #error Build the installer through tools\release.ps1, which passes AppVersion, SourceDir and OutputDir.
#endif
#ifndef OutputDir
  #error Build the installer through tools\release.ps1, which passes AppVersion, SourceDir and OutputDir.
#endif

; Pinned like the other release tools, since its Setup code ships inside the
; installer. The dark wizard with no pictures needs 6.7.0 or later, and a
; newer version gets the install and uninstall test before this changes.
#define InnoVersion "6.7.3"
#if DecodeVer(Ver) != InnoVersion
  #expr Error("This is Inno Setup " + DecodeVer(Ver) + ", and the installer is tested with " + InnoVersion + ". Install Inno Setup " + InnoVersion + " from jrsoftware.org for this user only, or test the installer with " + DecodeVer(Ver) + " and change InnoVersion in booth.iss.")
#endif

[Setup]
; The same for every version, so a newer installer replaces the older install.
AppId={{87DFEF29-7B44-4A76-8F33-43BA1EF8F04D}
AppName=Booth
AppVersion={#AppVersion}
AppVerName=Booth {#AppVersion}
AppPublisher=Shadi Alrashoodi
AppCopyright=Copyright (c) 2026 Shadi Alrashoodi
VersionInfoVersion={#AppVersion}
; Per user: Booth goes to %LOCALAPPDATA%\Programs\Booth and its entry to the
; user's own Start menu, which needs no administrator prompt. Without
; PrivilegesRequiredOverridesAllowed, /ALLUSERS cannot turn it into an
; install for everyone, which would need one.
PrivilegesRequired=lowest
DefaultDirName={autopf}\Booth
DisableDirPage=yes
DefaultGroupName=Booth
DisableProgramGroupPage=yes
UninstallDisplayName=Booth
UninstallDisplayIcon={app}\booth.exe
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 version 2004 (build 19041), the oldest Windows Booth runs on.
MinVersion=10.0.19041
OutputDir={#OutputDir}
OutputBaseFilename=booth-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
; Dark like the panel, and without Inno Setup's own pictures, which say
; nothing about Booth.
WizardStyle=modern dark
WizardImageFile=
WizardSmallImageFile=
; Keys, settings and known hosts live in %LOCALAPPDATA%\Booth, not here, so
; uninstalling leaves them for the next install.
;
; No firewall rule: adding one takes an administrator, and booth.exe asks for
; that itself, once, behind a screen that says why. The rule names booth.exe's
; path, so it stays in Windows after an uninstall until removed by hand.

[InstallDelete]
; A newer release can ship fewer DLLs or other texts in ffmpeg\, and what an
; older one left would make the folder differ from the zip.
Type: files; Name: "{app}\*.dll"
Type: filesandordirs; Name: "{app}\ffmpeg"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\Booth"; Filename: "{app}\booth.exe"

[Run]
; Off unless ticked, so the installer starts nothing on its own. Hidden when
; Setup itself runs as administrator: Booth would start as administrator
; too, which the panel refuses.
Filename: "{app}\booth.exe"; Description: "Start Booth"; Flags: nowait postinstall skipifsilent unchecked; Check: not IsAdmin

[Messages]
; Inno Setup's own texts say everything was removed. The keys stay on
; purpose, and someone uninstalling to get rid of them on a shared PC needs
; to know where they are. The headings are sentence case like the panel.
WizardReady=Ready to install
WizardPreparing=Preparing to install
FinishedHeadingLabel=[name] is installed
FinishedLabel=Open [name] from the Start menu.
UninstallAppFullTitle=Uninstall %1
ConfirmUninstall=Remove %1 from this PC?%n%nYour keys, settings and known hosts stay in %%LOCALAPPDATA%%\Booth for the next install. Delete that folder as well to remove them.
UninstalledAll=%1 was removed. Your keys, settings and known hosts are still in %%LOCALAPPDATA%%\Booth. Delete that folder to remove them.
UninstalledMost=%1 was removed, but some files in its folder could not be deleted. Delete them by hand. Your keys, settings and known hosts are still in %%LOCALAPPDATA%%\Booth.
