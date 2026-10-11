@echo off
rem Siterip Downloader build script
rem Usage:
rem   build.bat          build release only
rem   build.bat run      build release and start the app
rem   build.bat dev      fast syntax check only (cargo check, no exe)
rem NOTE: keep this file ASCII-only. cmd.exe reads .bat as ANSI/GBK,
rem       UTF-8 Chinese text here will corrupt parsing.
cd /d "%~dp0"

set EXE=target\release\siterip-downloader.exe

if /i "%1"=="dev" (
    echo [cargo check] fast syntax check, no exe produced...
    cargo check
    goto :done
)

rem The running app locks the exe: cargo compiles everything (~2 min) then dies
rem on "Access denied" at the final link step. Catch it before the wasted build.
tasklist | findstr /i "siterip-downloader.exe" >nul
if not errorlevel 1 (
    echo.
    echo WARNING: siterip-downloader.exe is still RUNNING. Close it first,
    echo or the build compiles for minutes and fails at the last step.
    choice /C YN /M "Kill it and continue"
    if errorlevel 2 (
        echo Aborted by user.
        pause
        exit /b 1
    )
    taskkill /IM siterip-downloader.exe /F >nul
)

echo [1/2] cargo build --release, takes 2-3 minutes after big changes...
cargo build --release
if errorlevel 1 (
    echo.
    echo BUILD FAILED. Send the error messages above to the developer.
    pause
    exit /b 1
)

echo [2/2] Build OK: %EXE%
dir "%EXE%" | findstr /i "siterip"

:done
if /i "%1"=="run" (
    echo Starting app...
    start "" "%EXE%"
) else (
    echo.
    echo Done. Double-click start.bat or run: build.bat run
)
pause
