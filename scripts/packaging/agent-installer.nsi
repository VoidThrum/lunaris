; Lunaris Agent installer for Windows.
; Built in CI from the repository root:
;   makensis /DVERSION=<x.y.z> scripts\packaging\agent-installer.nsi
; expecting the deployed agent (windeployqt + DLLs) in .\lunaris-agent\.

!include "MUI2.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif

!define APP     "Lunaris Agent"
!define EXE     "lunaris-agent.exe"
!define UNINST  "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP}"

Name "${APP}"
; One LZMA stream over all files: about a third smaller than the default.
SetCompressor /SOLID lzma
OutFile "..\..\lunaris-agent-installer.exe"
InstallDir "$PROGRAMFILES64\${APP}"
InstallDirRegKey HKLM "Software\${APP}" "InstallDir"
RequestExecutionLevel admin
Unicode true

!define MUI_ABORTWARNING
!define MUI_ICON   "..\..\agent\icons\icon.ico"
!define MUI_UNICON "..\..\agent\icons\icon.ico"
!define MUI_FINISHPAGE_TEXT "Lunaris Agent is installed.$\r$\n$\r$\nStart it from the Start menu, enter your server address and token, then click Start. Enable $\"Start at login$\" to keep this computer reachable."

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"


; ---- Safe install / uninstall -------------------------------------------------
; The uninstaller removes only what the installer put there (install.log), never a
; whole folder, so a mistaken install folder cannot lose the user's own files.
Var LogFile

; An existing install folder must be empty or a previous install of this app
; (uninstaller + main exe). Result in $InstDirOk.
Var InstDirOk
Function CheckInstDir
  Push $0
  Push $1
  StrCpy $InstDirOk 1
  IfFileExists "$INSTDIR\*.*" 0 cid_done
  IfFileExists "$INSTDIR\uninstall.exe" 0 cid_scan
  IfFileExists "$INSTDIR\${EXE}" cid_done cid_scan
  cid_scan:
  FindFirst $0 $1 "$INSTDIR\*.*"
  cid_loop:
    StrCmp $1 "" cid_empty
    StrCmp $1 "." cid_next
    StrCmp $1 ".." cid_next
    FindClose $0
    StrCpy $InstDirOk 0
    Goto cid_done
  cid_next:
    FindNext $0 $1
    Goto cid_loop
  cid_empty:
    FindClose $0
  cid_done:
  Pop $1
  Pop $0
FunctionEnd

Function .onVerifyInstDir
  Call CheckInstDir
  StrCmp $InstDirOk 1 +2
    Abort
FunctionEnd

; Writes every file ("F|rel") and folder ("D|rel", after its contents) under
; $INSTDIR\<rel> to the log. Argument on the stack: rel ("" for the root).
Function LogDir
  Exch $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  StrCpy $R3 "$INSTDIR"
  StrCmp $R0 "" +2
    StrCpy $R3 "$INSTDIR\$R0"
  FindFirst $R1 $R2 "$R3\*.*"
  ld_loop:
    StrCmp $R2 "" ld_end
    StrCmp $R2 "." ld_next
    StrCmp $R2 ".." ld_next
    StrCpy $R4 "$R2"
    StrCmp $R0 "" +2
      StrCpy $R4 "$R0\$R2"
    IfFileExists "$R3\$R2\*.*" ld_dir ld_file
  ld_dir:
    Push $R4
    Call LogDir
    Goto ld_next
  ld_file:
    StrCmp $R4 "install.log" ld_next
    StrCmp $R4 "uninstall.exe" ld_next
    FileWrite $LogFile "F|$R4$\n"
  ld_next:
    FindNext $R1 $R2
    Goto ld_loop
  ld_end:
  FindClose $R1
  StrCmp $R0 "" +2
    FileWrite $LogFile "D|$R0$\n"
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
FunctionEnd

; 1 on the stack if the log path is unsafe (absolute, drive, or ..), else 0.
Function un.BadRel
  Exch $R0
  Push $R1
  Push $R2
  StrCpy $R1 0
  StrCpy $R2 $R0 1
  StrCmp $R2 "" br_bad
  StrCmp $R2 "\" br_bad
  StrCmp $R2 "/" br_bad
  br_loop:
    StrCpy $R2 $R0 2 $R1
    StrCmp $R2 ".." br_bad
    StrCpy $R2 $R0 1 $R1
    StrCmp $R2 "" br_good
    StrCmp $R2 ":" br_bad
    IntOp $R1 $R1 + 1
    Goto br_loop
  br_bad:
    StrCpy $R0 1
    Goto br_end
  br_good:
    StrCpy $R0 0
  br_end:
  Pop $R2
  Pop $R1
  Exch $R0
FunctionEnd

Section "Install"
  Call CheckInstDir
  StrCmp $InstDirOk 1 +3
    MessageBox MB_OK|MB_ICONSTOP "The install folder $INSTDIR is not empty and is not a previous install. Choose an empty folder." /SD IDOK
    Abort
  ; Replace a running copy on upgrade.
  nsExec::Exec 'taskkill /IM "${EXE}" /F'

  SetOutPath "$INSTDIR"
  File /r "..\..\lunaris-agent\*"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "Software\${APP}" "InstallDir" "$INSTDIR"
  ; Record what was installed so the uninstaller removes exactly that.
  FileOpen $LogFile "$INSTDIR\install.log" w
  Push ""
  Call LogDir
  FileClose $LogFile

  CreateDirectory "$SMPROGRAMS\${APP}"
  CreateShortcut "$SMPROGRAMS\${APP}\${APP}.lnk" "$INSTDIR\${EXE}"
  CreateShortcut "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk" "$INSTDIR\uninstall.exe"
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\${EXE}"

  ; Let peers reach the agent directly (WebRTC P2P / WebTransport).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${APP}"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="${APP}" dir=in action=allow program="$INSTDIR\${EXE}" enable=yes'

  WriteRegStr HKLM "${UNINST}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINST}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST}" "Publisher" "Lunaris"
  WriteRegStr HKLM "${UNINST}" "DisplayIcon" '"$INSTDIR\${EXE}"'
  WriteRegStr HKLM "${UNINST}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST}" "URLInfoAbout" "https://github.com/collyn/lunaris"
  WriteRegDWORD HKLM "${UNINST}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  nsExec::Exec 'taskkill /IM "${EXE}" /F'
  ; "Start at login" entry written by the agent (current user).
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "LunarisAgent"
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${APP}"'

  Delete "$SMPROGRAMS\${APP}\${APP}.lnk"
  Delete "$SMPROGRAMS\${APP}\Uninstall ${APP}.lnk"
  RMDir "$SMPROGRAMS\${APP}"
  Delete "$DESKTOP\${APP}.lnk"

  DeleteRegKey HKLM "${UNINST}"
  DeleteRegKey HKLM "Software\${APP}"
  ; Remove what the installer recorded, then the folder only if it is now empty.
  IfFileExists "$INSTDIR\install.log" 0 un_nolog
  FileOpen $0 "$INSTDIR\install.log" r
  un_loop:
    FileRead $0 $1
    StrCmp $1 "" un_done
  un_trim:
    StrCpy $2 $1 1 -1
    StrCmp $2 "$\n" un_cut
    StrCmp $2 "$\r" un_cut
    Goto un_trimmed
  un_cut:
    StrCpy $1 $1 -1
    Goto un_trim
  un_trimmed:
    StrCpy $2 $1 2
    StrCpy $3 $1 "" 2
    Push $3
    Call un.BadRel
    Pop $4
    StrCmp $4 1 un_loop
    StrCmp $2 "F|" un_file
    StrCmp $2 "D|" un_dir
    Goto un_loop
  un_file:
    Delete "$INSTDIR\$3"
    Goto un_loop
  un_dir:
    RMDir "$INSTDIR\$3"
    Goto un_loop
  un_done:
  FileClose $0
  Delete "$INSTDIR\install.log"
  un_nolog:
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; Settings in %APPDATA%\lunaris-agent are kept.
SectionEnd
