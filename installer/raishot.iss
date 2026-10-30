; RaiShot 安装包：按当前用户安装、不需要管理员权限，文件与便携 ZIP 相同。
; 由 Release 工作流用 ISCC 编译：
;   ISCC /DAppVersion=X.Y.Z /DSourceDir=<便携包展开目录> /DOutputDir=<输出目录> installer\raishot.iss
; 品牌值须与 src/brand.rs 一致：EXE 名、数据目录 %LOCALAPPDATA%\RaiShot、Run 值名 RaiShot。
; 设计与升级／卸载约定见 docs/releasing.md 的“安装包”一节。

#ifndef AppVersion
  #error 缺少 /DAppVersion=X.Y.Z
#endif
#ifndef SourceDir
  #error 缺少 /DSourceDir=<便携包展开目录>
#endif
#ifndef OutputDir
  #define OutputDir "..\dist\installer"
#endif

#define AppName "RaiShot"
; 固定 AppId：升级识别同一产品，永不更改。
#define AppGuid "6B0F3C52-8E7A-4D1B-9F25-3A6C1E4D7B90"
#define ResidentExe "raishot.exe"
#define CliExe "raishot-cli.exe"
#define RunValue "RaiShot"
#define DataDirName "RaiShot"

[Setup]
AppId={{{#AppGuid}}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
AppPublisherURL=https://github.com/ceastld/raishot
AppSupportURL=https://github.com/ceastld/raishot/issues
AppUpdatesURL=https://github.com/ceastld/raishot/releases
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
; 只按当前用户安装：Run 项、App Paths、低级 Hook 都按用户生效，不需要 UAC。
PrivilegesRequired=lowest
; lowest 下 {autopf} 即 %LOCALAPPDATA%\Programs；不能装进数据目录 %LOCALAPPDATA%\RaiShot。
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
UsePreviousTasks=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows.Graphics.Capture 需要 1903 及以上。
MinVersion=10.0.18362
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\app.ico
UninstallDisplayName={#AppName}
OutputDir={#OutputDir}
OutputBaseFilename={#AppName}-{#AppVersion}-setup-x64
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=auto
; 托盘先由 PrepareToInstall 调用 --exit 正常退出；仍占用文件的进程再交给 Restart Manager。
CloseApplications=yes
RestartApplications=no
ChangesEnvironment=yes
SetupLogging=yes

[Languages]
Name: "zh"; MessagesFile: "ChineseSimplified.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
zh.AutoStart=登录 Windows 时启动 {#AppName}
en.AutoStart=Start {#AppName} when I sign in to Windows
zh.AddToPath=将命令行工具 {#CliExe} 加入用户 PATH
en.AddToPath=Add the {#CliExe} command-line tool to the user PATH
zh.Shortcut=截图、贴图、OCR 与录屏
en.Shortcut=Screenshot, pin, OCR and screen recording
zh.DataDirConflict=不能安装到数据目录 %1，请选择其它文件夹。
en.DataDirConflict=Cannot install into the data folder %1. Choose another folder.
zh.DeleteUserData=是否同时删除 {#AppName} 的设置、贴图、历史与 OCR 缓存？%n%n%1%n%n“图片”文件夹中保存的截图不会被删除。
en.DeleteUserData=Also delete {#AppName} settings, pins, history and OCR cache?%n%n%1%n%nScreenshots saved in your Pictures folder are kept.

[Tasks]
; 开机启动只在首次安装时提供；升级保留用户在应用设置中的选择。
Name: "autostart"; Description: "{cm:AutoStart}"; Check: not IsUpgrade
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "clipath"; Description: "{cm:AddToPath}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\assets\app.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#ResidentExe}"; Parameters: "--tray"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"; Comment: "{cm:Shortcut}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#ResidentExe}"; Parameters: "--tray"; WorkingDir: "{app}"; IconFilename: "{app}\app.ico"; Comment: "{cm:Shortcut}"; Tasks: desktopicon

[Registry]
; Win+R、ShellExecute 与 Quicker 动作可直接按名称调用托盘入口。
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#ResidentExe}"; ValueType: string; ValueName: ""; ValueData: "{app}\{#ResidentExe}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\{#ResidentExe}"; ValueType: string; ValueName: "Path"; ValueData: "{app}"

[Run]
Filename: "{app}\{#ResidentExe}"; Parameters: "--tray"; WorkingDir: "{app}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  ApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';
  EnvKey = 'Environment';
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{{#AppGuid}}_is1';

function IsUpgrade: Boolean;
begin
  Result := RegValueExists(HKCU, UninstallKey, 'UninstallString');
end;

function DataDir: String;
begin
  Result := ExpandConstant('{localappdata}\{#DataDirName}');
end;

function SamePath(A, B: String): Boolean;
begin
  Result := CompareText(RemoveBackslashUnlessRoot(A), RemoveBackslashUnlessRoot(B)) = 0;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Dir: String;
begin
  Result := True;
  if CurPageID = wpSelectDir then
  begin
    Dir := WizardDirValue;
    // 程序目录在卸载时整体删除，不能与数据目录重叠。
    if SamePath(Dir, DataDir) or (Pos(Lowercase(AddBackslash(DataDir)), Lowercase(AddBackslash(Dir))) = 1) then
    begin
      MsgBox(FmtMessage(CustomMessage('DataDirConflict'), [DataDir]), mbError, MB_OK);
      Result := False;
    end;
  end;
end;

// 请本会话中的托盘（安装版或便携版）走正常退出并保存贴图工作区；没有托盘时立即返回。
procedure ExitResident(Exe: String);
var
  Code: Integer;
begin
  if FileExists(Exe) then
    if not Exec(Exe, '--exit', '', SW_HIDE, ewWaitUntilTerminated, Code) then
      Log('--exit failed to start: ' + SysErrorMessage(Code))
    else
      Log('--exit exit code: ' + IntToStr(Code));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  // 用本次安装包里的新程序发请求，旧版本没有 --exit 也能关闭；依赖的 VC++ 运行库一并解出。
  try
    ExtractTemporaryFile('vcruntime140.dll');
    ExtractTemporaryFile('vcruntime140_1.dll');
    ExtractTemporaryFile('msvcp140.dll');
    ExtractTemporaryFile('msvcp140_1.dll');
    ExtractTemporaryFile('{#ResidentExe}');
    ExitResident(ExpandConstant('{tmp}\{#ResidentExe}'));
  except
    Log('--exit skipped: ' + GetExceptionMessage);
  end;
end;

procedure AddToUserPath(Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Paths := '';
  if Pos(';' + Lowercase(RemoveBackslashUnlessRoot(Dir)) + ';', ';' + Lowercase(Paths) + ';') > 0 then
    Exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths + RemoveBackslashUnlessRoot(Dir));
end;

procedure RemoveFromUserPath(Dir: String);
var
  Paths, Rest, Item, Kept: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Exit;
  Rest := Paths + ';';
  Kept := '';
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    Item := Copy(Rest, 1, P - 1);
    Delete(Rest, 1, P);
    if (Item <> '') and not SamePath(Item, Dir) then
    begin
      if Kept <> '' then
        Kept := Kept + ';';
      Kept := Kept + Item;
    end;
  end;
  if Kept <> Paths then
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Kept);
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  App: String;
begin
  if CurStep <> ssPostInstall then
    Exit;
  App := ExpandConstant('{app}');
  // 与 src/autostart.rs 的 command_line 相同格式，设置页才会识别为“已开启”。
  if WizardIsTaskSelected('autostart') then
    RegWriteStringValue(HKCU, RunKey, '{#RunValue}', '"' + AddBackslash(App) + '{#ResidentExe}" --tray');
  if WizardIsTaskSelected('clipath') then
    AddToUserPath(App)
  else
    RemoveFromUserPath(App);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  App, Command: String;
begin
  App := ExpandConstant('{app}');
  case CurUninstallStep of
    usUninstall:
      ExitResident(AddBackslash(App) + '{#ResidentExe}');
    usPostUninstall:
      begin
        // 只删除指向本安装目录的启动项；便携版等其它位置的登记保持不变。
        if RegQueryStringValue(HKCU, RunKey, '{#RunValue}', Command) and
           (Pos(Lowercase(AddBackslash(App)), Lowercase(Command)) > 0) then
        begin
          RegDeleteValue(HKCU, RunKey, '{#RunValue}');
          RegDeleteValue(HKCU, ApprovedKey, '{#RunValue}');
        end;
        RemoveFromUserPath(App);
        // 用户数据默认保留；静默卸载一律保留。
        if not UninstallSilent and DirExists(DataDir) then
          if MsgBox(FmtMessage(CustomMessage('DeleteUserData'), [DataDir]), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
            DelTree(DataDir, True, True, True);
      end;
  end;
end;
