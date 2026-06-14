; NSIS installer script for anodizer (Windows .exe installer).
; Rendered through anodizer's template engine before makensis runs, so the
; double-brace placeholders below are filled per published crate/target: ProjectName,
; NsisOutputFile (absolute output path makensis writes to), ProgramFiles
; (arch-aware $PROGRAMFILES64 / $PROGRAMFILES), NsisBinaryPath (staged source
; binary), and NsisBinaryName (the .exe filename). makensis itself is preinstalled
; in the release base image (apt: nsis); cross-platform, builds on Linux CI.

!include "MUI2.nsh"

Name "{{ ProjectName }}"
OutFile "{{ NsisOutputFile }}"
Unicode true

; Per-machine install under Program Files; admin is required to write there and
; to add the install dir to the system PATH below.
InstallDir "{{ ProgramFiles }}\{{ ProjectName }}"
RequestExecutionLevel admin

!define MUI_ABORTWARNING

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\{{ ProjectName }}"

Section "Install"
    SetOutPath "$INSTDIR"
    File "{{ NsisBinaryPath }}"
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Start Menu shortcut so the CLI is discoverable from the shell.
    CreateDirectory "$SMPROGRAMS\{{ ProjectName }}"
    CreateShortCut "$SMPROGRAMS\{{ ProjectName }}\{{ ProjectName }}.lnk" "$INSTDIR\{{ NsisBinaryName }}"

    ; Add/Remove Programs registration so the installer is uninstallable from the
    ; Windows Settings UI, not just the bundled uninstall.exe.
    WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "{{ ProjectName }}"
    WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "TJ Smith"
    WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
    WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\{{ NsisBinaryName }}"
    Delete "$INSTDIR\uninstall.exe"
    Delete "$SMPROGRAMS\{{ ProjectName }}\{{ ProjectName }}.lnk"
    RMDir "$SMPROGRAMS\{{ ProjectName }}"
    RMDir "$INSTDIR"
    DeleteRegKey HKLM "${UNINST_KEY}"
SectionEnd
