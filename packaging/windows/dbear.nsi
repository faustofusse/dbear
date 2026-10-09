; dbear for Windows: per-user installer (no administrator rights), built by scripts/package-windows.sh.
;
;   makensis -DVERSION=1.2.0 -DARCH=x64 -DSRCDIR=<folder with dbear.exe> -DOUTFILE=<setup.exe> dbear.nsi
;
; Installs into %LOCALAPPDATA%\Programs\dbear, adds a Start menu shortcut and an entry in
; Settings > Apps. User data (%APPDATA%\dbear: connections, state) is never touched; passwords
; live in the Credential Manager.
;
; The app updates itself by running this installer silently:
;   dbear-<version>-setup.exe /S /UPDATE [/RELAUNCH] /D=<install folder>
; /UPDATE waits for the running dbear.exe to exit; /RELAUNCH starts the new version afterwards.

!ifndef VERSION
  !error "pass -DVERSION=x.y.z"
!endif
!ifndef ARCH
  !define ARCH x64
!endif
!ifndef SRCDIR
  !error "pass -DSRCDIR=<folder with dbear.exe>"
!endif
!ifndef OUTFILE
  !define OUTFILE "dbear-${VERSION}-windows-${ARCH}-setup.exe"
!endif
; VIProductVersion needs four numbers; pre-release suffixes are dropped (scripts pass NUMVERSION).
!ifndef NUMVERSION
  !define NUMVERSION "0.0.0.0"
!endif

!define APPNAME "dbear"
!define PUBLISHER "Fausto Fusse"
!define URL "https://github.com/faustofusse/dbear"
!define UNINSTALL_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\dbear"
!define APP_KEY "Software\dbear"

Unicode true
ManifestDPIAware true
SetCompressor /SOLID lzma
RequestExecutionLevel user
Name "${APPNAME}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\dbear"
InstallDirRegKey HKCU "${APP_KEY}" "InstallDir"
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${NUMVERSION}"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "CompanyName" "${PUBLISHER}"
VIAddVersionKey "LegalCopyright" "Copyright (c) Fausto Fusse. MIT License."

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "LogicLib.nsh"

!define MUI_ICON "dbear.ico"
!define MUI_UNICON "dbear.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\dbear.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open dbear"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Var Relaunch

Function .onInit
  ${GetParameters} $R0
  StrCpy $Relaunch 0
  ; /UPDATE needs nothing extra yet: it marks runs by the app (silent, waiting for it to quit).
  ClearErrors
  ${GetOptions} $R0 "/RELAUNCH" $R1
  ${IfNot} ${Errors}
    StrCpy $Relaunch 1
  ${EndIf}
FunctionEnd

; Waits until $INSTDIR\dbear.exe can be replaced (a running exe can't be deleted). Updates wait up
; to a minute for the app that started them to quit; interactive installs ask to close it.
!macro WaitForApp un
Function ${un}WaitForApp
  ${IfNot} ${FileExists} "$INSTDIR\dbear.exe"
    Return
  ${EndIf}
  StrCpy $R2 0
  retry:
    ClearErrors
    Delete "$INSTDIR\dbear.exe"
    ${IfNot} ${Errors}
      Return
    ${EndIf}
    IntOp $R2 $R2 + 1
    ${If} $R2 < 120
      Sleep 500
      Goto retry
    ${EndIf}
    ${If} ${Silent}
      SetErrorLevel 5
      Quit
    ${EndIf}
    MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "dbear is running. Quit it, then click Retry." /SD IDCANCEL IDRETRY again
    Quit
  again:
    StrCpy $R2 100
    Goto retry
FunctionEnd
!macroend
!insertmacro WaitForApp ""
!insertmacro WaitForApp "un."

Section "dbear" SecMain
  SetShellVarContext current
  SetOutPath "$INSTDIR"
  Call WaitForApp

  File "${SRCDIR}\dbear.exe"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\dbear.lnk" "$INSTDIR\dbear.exe" "" "$INSTDIR\dbear.exe" 0

  WriteRegStr HKCU "${APP_KEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "URLInfoAbout" "${URL}"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "DisplayIcon" "$INSTDIR\dbear.exe,0"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTALL_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINSTALL_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTALL_KEY}" "EstimatedSize" "$0"

  ; Leftovers of the download the app ran this from are cleaned up by the app itself.
  ${If} $Relaunch == 1
    Exec '"$INSTDIR\dbear.exe"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  Call un.WaitForApp
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\dbear.lnk"
  DeleteRegKey HKCU "${UNINSTALL_KEY}"
  DeleteRegKey HKCU "${APP_KEY}"
SectionEnd
