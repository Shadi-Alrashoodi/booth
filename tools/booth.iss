; The optional installer. The zip stays the main way to get Booth. This puts
; the same files in the user's own programs folder, adds a Start menu entry,
; a desktop shortcut unless its box is cleared, and an uninstaller, and never
; asks for administrator rights.
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
; installer. The dark wizard without bevels and in the window tone needs
; 6.7.0 or later, and a newer version gets the install and uninstall test
; before this changes.
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
; The mark on the setup file, its window and the uninstaller.
SetupIconFile=..\crates\app\assets\booth.ico
; Dark like the panel, and without the lines across the pages, which the
; panel no longer draws either. No picture on the last page, since it would
; say nothing about Booth and push that page's text away from the left edge
; every other page uses ([Code] lays the page out like the rest). The corner picture is the mark on window tone,
; drawn on whole pixels at each size Setup asks for from 100 to 250 percent,
; so it picks one instead of scaling one soft. The pages and the space
; around the picture take the same window tone, #141412, so the picture's
; own square does not show against the dark style's grey.
WizardStyle=modern dark hidebevels
WizardBackColor=#141412
WizardImageFile=
WizardSmallImageFile=installer\mark-58.png,installer\mark-77.png,installer\mark-97.png,installer\mark-116.png,installer\mark-124.png,installer\mark-143.png,installer\mark-159.png
WizardSmallImageBackColor=#141412
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

[Tasks]
; Ticked, and a silent install makes the shortcut too. A package manager
; that wants none passes /MERGETASKS="!desktopicon".
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"

[Icons]
Name: "{autoprograms}\Booth"; Filename: "{app}\booth.exe"
Name: "{autodesktop}\Booth"; Filename: "{app}\booth.exe"; Tasks: desktopicon

[Run]
; Ticked, so Booth opens when Setup closes, but never after a silent
; install, where a package manager starts nothing it did not ask for. Hidden
; when Setup itself runs as administrator: Booth would start as
; administrator too, which the panel refuses.
Filename: "{app}\booth.exe"; Description: "Start Booth"; Flags: nowait postinstall skipifsilent; Check: not IsAdmin

[Messages]
; Inno Setup's own texts say everything was removed. The keys stay on
; purpose, and someone uninstalling to get rid of them on a shared PC needs
; to know where they are. Its other texts follow the panel too: headings
; and titles in sentence case, plain Back and Next with no arrows, and "this
; PC" where they said "your computer".
WizardSelectTasks=Select additional tasks
WizardReady=Ready to install
WizardPreparing=Preparing to install
WizardUninstalling=Uninstall status
ExitSetupTitle=Exit setup
ExitSetupMessage=If you exit now, nothing is installed. You can run Setup again later.%n%nExit Setup?
ButtonBack=&Back
ButtonNext=&Next
SelectTasksDesc=What else should Setup do?
SelectTasksLabel2=Clear any box you do not want, then click Next.
ApplicationsFound=These programs use files Setup has to replace. Let Setup close them, or close them yourself first.
ApplicationsFound2=These programs use files Setup has to replace. Let Setup close them, and it starts them again when it is done.
CloseApplications=&Close them for me
DontCloseApplications=&Leave them open
ReadyLabel1=Setup is ready to install [name] on this PC.
ReadyLabel2a=Click Install to start, or Back to change something.
PreparingDesc=Setup is preparing to install [name] on this PC.
InstallingLabel=Setup is installing [name] on this PC.
UninstallStatusLabel=Removing %1 from this PC.
FinishedHeadingLabel=[name] is installed
FinishedLabel=Open Booth from the Start menu or the desktop shortcut.
UninstallAppFullTitle=Uninstall %1
ConfirmUninstall=Remove %1 from this PC?%n%nYour keys, settings and known hosts stay in %%LOCALAPPDATA%%\Booth for the next install. Delete that folder as well to remove them.
UninstalledAll=%1 was removed. Your keys, settings and known hosts are still in %%LOCALAPPDATA%%\Booth. Delete that folder to remove them.
UninstalledMost=%1 was removed, but some files in its folder could not be deleted. Delete them by hand. Your keys, settings and known hosts are still in %%LOCALAPPDATA%%\Booth.

[CustomMessages]
FinishedLabelNoDesktop=Open Booth from the Start menu.

[Code]
// One layout on every page: the heading, the line under it and the page's
// own text all start at the heading's left edge, and the mark sits top
// right. Inno Setup lays out its last page differently, a picture column
// with the text beside it, which left the text floating in the middle of
// the window. That page is laid out here like the others instead: no
// picture, the heading where the other pages have theirs and in their font,
// the text where theirs starts, and the mark moved onto it.
procedure InitializeWizard;
var
  Edge, Shift: Integer;
begin
  Edge := WizardForm.PageNameLabel.Left;
  Shift := WizardForm.PageDescriptionLabel.Left - Edge;
  WizardForm.PageDescriptionLabel.Left := Edge;
  WizardForm.PageDescriptionLabel.Width := WizardForm.PageDescriptionLabel.Width + Shift;
  Shift := WizardForm.InnerNotebook.Left - Edge;
  WizardForm.InnerNotebook.Left := Edge;
  WizardForm.InnerNotebook.Width := WizardForm.InnerNotebook.Width + Shift;
end;

procedure LayOutFinishedPage;
var
  Edge, Gap: Integer;
begin
  Edge := WizardForm.PageNameLabel.Left;
  WizardForm.WizardBitmapImage2.Visible := False;
  WizardForm.WizardSmallBitmapImage.Parent := WizardForm.FinishedPage;

  WizardForm.FinishedHeadingLabel.Font.Name := WizardForm.PageNameLabel.Font.Name;
  WizardForm.FinishedHeadingLabel.Font.Size := WizardForm.PageNameLabel.Font.Size;
  WizardForm.FinishedHeadingLabel.Font.Style := WizardForm.PageNameLabel.Font.Style;
  WizardForm.FinishedHeadingLabel.Left := Edge;
  WizardForm.FinishedHeadingLabel.Top := WizardForm.PageNameLabel.Top;
  WizardForm.FinishedHeadingLabel.Width := WizardForm.WizardSmallBitmapImage.Left - Edge - ScaleX(8);
  WizardForm.FinishedHeadingLabel.AdjustHeight;

  Gap := WizardForm.RunList.Top - WizardForm.FinishedLabel.Top;
  WizardForm.FinishedLabel.Left := Edge;
  WizardForm.FinishedLabel.Top := WizardForm.InnerNotebook.Top;
  WizardForm.FinishedLabel.Width := WizardForm.InnerNotebook.Width;
  WizardForm.RunList.Left := Edge;
  WizardForm.RunList.Top := WizardForm.FinishedLabel.Top + Gap;
  WizardForm.RunList.Width := WizardForm.InnerNotebook.Width;
  // Its item was drawn dark on the dark page, so the box showed with no
  // words beside it; the task list on the first page draws its items right.
  WizardForm.RunList.Color := WizardForm.TasksList.Color;
  WizardForm.RunList.Font.Color := WizardForm.TasksList.Font.Color;
end;

// The uninstaller's page, the same way.
procedure InitializeUninstallProgressForm;
var
  Edge, Shift: Integer;
begin
  Edge := UninstallProgressForm.PageNameLabel.Left;
  Shift := UninstallProgressForm.PageDescriptionLabel.Left - Edge;
  UninstallProgressForm.PageDescriptionLabel.Left := Edge;
  UninstallProgressForm.PageDescriptionLabel.Width := UninstallProgressForm.PageDescriptionLabel.Width + Shift;
  Shift := UninstallProgressForm.InnerNotebook.Left + UninstallProgressForm.StatusLabel.Left - Edge;
  UninstallProgressForm.StatusLabel.Left := UninstallProgressForm.StatusLabel.Left - Shift;
  UninstallProgressForm.StatusLabel.Width := UninstallProgressForm.StatusLabel.Width + Shift;
  UninstallProgressForm.ProgressBar.Left := UninstallProgressForm.ProgressBar.Left - Shift;
  UninstallProgressForm.ProgressBar.Width := UninstallProgressForm.ProgressBar.Width + Shift;
end;

// The finish text points at the desktop shortcut, which is not there when
// its box was cleared. Literal text in FinishedLabel, not [name], so it is
// found as written.
procedure CurPageChanged(CurPageID: Integer);
var
  Text: String;
begin
  if CurPageID = wpFinished then begin
    if not WizardIsTaskSelected('desktopicon') then begin
      Text := WizardForm.FinishedLabel.Caption;
      StringChangeEx(Text, SetupMessage(msgFinishedLabel), CustomMessage('FinishedLabelNoDesktop'), True);
      WizardForm.FinishedLabel.Caption := Text;
    end;
    LayOutFinishedPage;
  end;
end;