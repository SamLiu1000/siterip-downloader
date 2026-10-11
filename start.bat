@echo off
rem Siterip Downloader launcher (double-click to run)
cd /d "%~dp0"

set EXE=target\release\siterip-downloader.exe

rem Build release binary if missing, OR if src/ui sources are newer than the exe
if not exist "%EXE%" (
    echo Building siterip-downloader, please wait...
    cargo build --release
    if errorlevel 1 (
        echo Build failed. Check your Rust toolchain.
        pause
        exit /b 1
    )
) else (
    powershell -NoProfile -Command "exit ([int]((Get-ChildItem -Recurse src,ui -File | Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime -gt (Get-Item '%EXE%').LastWriteTime))" >nul 2>nul
    if errorlevel 1 (
        echo Sources changed, rebuilding...
        cargo build --release
        if errorlevel 1 (
            echo Build failed. Check your Rust toolchain.
            pause
            exit /b 1
        )
    )
)

rem Check yt-dlp download engine
where yt-dlp >nul 2>nul
if errorlevel 1 (
    python -m yt_dlp --version >nul 2>nul
    if errorlevel 1 (
        echo yt-dlp not found, installing via pip...
        pip install -U yt-dlp
    )
)

start "" "%EXE%"
