; Lambo PHP - Windows installer (NSIS)
;
; Built by scripts/installer.ps1, which passes:
;   /DVERSION=0.9.0  /DBINARY=path\to\lambo.exe  /DOUTFILE=path\to\LamboPHP-Setup.exe
;   /DGUI=path\to\lambo-gui.exe   (optional; the installer works without it)
;
; Design notes:
;   * Per-user install into %LOCALAPPDATA%\Programs\LamboPHP, so the whole
;     install runs without administrator rights - the same promise the product
;     makes about ports and services.
;   * No data directory is created and nothing is written outside the install
;     directory, the user PATH and the Start Menu. Lambo creates its own home
;     (%USERPROFILE%\Lambo) the first time it runs, so uninstalling leaves the
;     user's PHP versions and databases alone.

Unicode true

; The version is release metadata: it is written to Add/Remove Programs and
; to HKCU\Software\LamboPHP. Falling back to a placeholder would let a
; mis-invoked build ship an installer that calls itself the wrong thing.
!ifndef VERSION
  !error "VERSION must be the release version, e.g. /DVERSION=0.13.0-rc.1"
!endif
!ifndef BINARY
  !error "BINARY must point at lambo.exe"
!endif

Name "Lambo PHP ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\LamboPHP"
InstallDirRegKey HKCU "Software\LamboPHP" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!include "MUI2.nsh"
!include "FileFunc.nsh"

!define MUI_PRODUCT "Lambo PHP"
!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\LICENSE-APACHE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!ifdef GUI
  !define MUI_FINISHPAGE_RUN "$INSTDIR\lambo-gui.exe"
!else
  !define MUI_FINISHPAGE_RUN "$INSTDIR\lambo.exe"
!endif
!define MUI_FINISHPAGE_RUN_PARAMETERS "doctor"
!define MUI_FINISHPAGE_RUN_TEXT "Run lambo doctor now"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Lambo PHP" SecLambo
  SectionIn RO

  SetOutPath "$INSTDIR"
  File "${BINARY}"
!ifdef GUI
  ; The desktop application. Optional so a CLI-only build still installs:
  ; the engine and the CLI are the product, the GUI is a view over them.
  File "${GUI}"
!endif
  File "..\LICENSE-APACHE"
  File "..\LICENSE-MIT"

  ; The user PATH, never the machine PATH: a per-user install has no business
  ; editing system-wide state, and doing so would demand elevation.
  ReadRegStr $0 HKCU "Environment" "Path"
  ${If} $0 == ""
    WriteRegExpandStr HKCU "Environment" "Path" "$INSTDIR"
  ${ElseIfNot} $0 == "*$INSTDIR*"
    WriteRegExpandStr HKCU "Environment" "Path" "$0;$INSTDIR"
  ${EndIf}

  WriteRegStr HKCU "Software\LamboPHP" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\LamboPHP" "Version" "${VERSION}"

  CreateDirectory "$SMPROGRAMS\Lambo PHP"
!ifdef GUI
  CreateShortcut "$SMPROGRAMS\Lambo PHP\Lambo PHP.lnk" "$INSTDIR\lambo-gui.exe"
!endif
  CreateShortcut "$SMPROGRAMS\Lambo PHP\Lambo doctor.lnk" "$INSTDIR\lambo.exe" "doctor"
  CreateShortcut "$SMPROGRAMS\Lambo PHP\Uninstall Lambo PHP.lnk" "$INSTDIR\uninstall.exe"

  WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\lambo.exe"
!ifdef GUI
  Delete "$INSTDIR\lambo-gui.exe"
!endif
  Delete "$INSTDIR\LICENSE-APACHE"
  Delete "$INSTDIR\LICENSE-MIT"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

!ifdef GUI
  Delete "$SMPROGRAMS\Lambo PHP\Lambo PHP.lnk"
!endif
  Delete "$SMPROGRAMS\Lambo PHP\Lambo doctor.lnk"
  Delete "$SMPROGRAMS\Lambo PHP\Uninstall Lambo PHP.lnk"
  RMDir "$SMPROGRAMS\Lambo PHP"

  ; Removes our entry from the user PATH and nothing else.
  ReadRegStr $0 HKCU "Environment" "Path"
  ${IfNot} $0 == ""
    ${WordReplace} $0 "$INSTDIR;" "" "+" $1
    ${WordReplace} $1 ";$INSTDIR" "" "+" $2
    ${WordReplace} $2 "$INSTDIR" "" "+" $3
    WriteRegExpandStr HKCU "Environment" "Path" "$3"
  ${EndIf}

  DeleteRegKey HKCU "Software\LamboPHP"
SectionEnd
