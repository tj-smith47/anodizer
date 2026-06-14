; NSIS installer script for anodizer (Windows .exe installer).
; Rendered through anodizer's template engine before makensis runs, so the
; double-brace placeholders below are filled per published crate/target: ProjectName,
; NsisOutputFile (absolute output path makensis writes to), ProgramFiles
; (arch-aware $PROGRAMFILES64 / $PROGRAMFILES), NsisBinaryPath (staged source
; binary), and NsisBinaryName (the .exe filename). makensis itself is preinstalled
; in the release base image (apt: nsis); cross-platform, builds on Linux CI.

!include "MUI2.nsh"
!include "WinMessages.nsh"
!include "LogicLib.nsh"

Name "{{ ProjectName }}"
OutFile "{{ NsisOutputFile }}"
Unicode true

; Per-machine install under Program Files; admin is required to write there and
; to add the install dir to the system (HKLM) PATH below.
InstallDir "{{ ProgramFiles }}\{{ ProjectName }}"
RequestExecutionLevel admin

; System-wide Environment key whose PATH value the installer extends so the CLI
; is invocable from any shell after install (it is a command-line tool, not a
; GUI app; a Start-Menu shortcut alone would not put it on PATH).
!define ENV_KEY "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"

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

    ; Prepend $INSTDIR to the system PATH (idempotent: skip if already present),
    ; then broadcast WM_SETTINGCHANGE so already-open shells pick it up without a
    ; reboot. WriteRegExpandStr keeps PATH a REG_EXPAND_SZ (preserving any
    ; %VAR% entries already in it).
    ReadRegStr $0 HKLM "${ENV_KEY}" "Path"
    ${If} $0 == ""
        WriteRegExpandStr HKLM "${ENV_KEY}" "Path" "$INSTDIR"
    ${Else}
        Push "$0"
        Push "$INSTDIR"
        Call StrContains
        Pop $1
        ${If} $1 == ""
            WriteRegExpandStr HKLM "${ENV_KEY}" "Path" "$INSTDIR;$0"
        ${EndIf}
    ${EndIf}
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
SectionEnd

; Pop a needle then a haystack; push the needle back if found, else empty.
; Used to keep the PATH edit idempotent across re-installs.
Function StrContains
    Exch $R0 ; needle
    Exch
    Exch $R1 ; haystack
    Push $R2
    Push $R3
    StrLen $R3 $R0
    StrCpy $R2 0
    loop:
        StrCpy $R4 $R1 $R3 $R2
        ${If} $R4 == $R0
            StrCpy $R0 $R0
            Goto done
        ${EndIf}
        StrCpy $R4 $R1 1 $R2
        ${If} $R4 == ""
            StrCpy $R0 ""
            Goto done
        ${EndIf}
        IntOp $R2 $R2 + 1
        Goto loop
    done:
    Pop $R3
    Pop $R2
    Pop $R1
    Exch $R0
FunctionEnd

Section "Uninstall"
    Delete "$INSTDIR\{{ NsisBinaryName }}"
    Delete "$INSTDIR\uninstall.exe"
    Delete "$SMPROGRAMS\{{ ProjectName }}\{{ ProjectName }}.lnk"
    RMDir "$SMPROGRAMS\{{ ProjectName }}"
    RMDir "$INSTDIR"
    DeleteRegKey HKLM "${UNINST_KEY}"

    ; Remove $INSTDIR from the system PATH (both leading and embedded forms) and
    ; broadcast the change so shells drop the stale entry.
    ReadRegStr $0 HKLM "${ENV_KEY}" "Path"
    Push "$0"
    Push "$INSTDIR;"
    Call un.StrReplace
    Pop $0
    Push "$0"
    Push "$INSTDIR"
    Call un.StrReplace
    Pop $0
    WriteRegExpandStr HKLM "${ENV_KEY}" "Path" "$0"
    SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
SectionEnd

; Pop a search string then a source string; push the source with the first
; occurrence of the search removed. Uninstall-namespaced (un.) for the Uninstall
; section.
Function un.StrReplace
    Exch $R0 ; search
    Exch
    Exch $R1 ; source
    Push $R2
    Push $R3
    Push $R4
    Push $R5
    StrLen $R2 $R0
    StrCpy $R3 0
    StrCpy $R5 ""
    loop:
        StrCpy $R4 $R1 $R2 $R3
        ${If} $R4 == $R0
            StrCpy $R4 $R1 $R3
            IntOp $R3 $R3 + $R2
            StrCpy $R1 $R1 "" $R3
            StrCpy $R1 "$R4$R1"
            Goto done
        ${EndIf}
        StrCpy $R4 $R1 1 $R3
        ${If} $R4 == ""
            Goto done
        ${EndIf}
        IntOp $R3 $R3 + 1
        Goto loop
    done:
    StrCpy $R0 $R1
    Pop $R5
    Pop $R4
    Pop $R3
    Pop $R2
    Pop $R1
    Exch $R0
FunctionEnd
