@echo off
REM Supervisor loop for personal-agent (Windows).
REM
REM - Runs `cargo run`.
REM - If the app exits with 255 (Rust's exit(-1)), rebuild and rerun.
REM - If the app exits with 0 (graceful shutdown, e.g. Ctrl+C), stop.
REM - Any other exit code is treated as an error and stops the loop.

setlocal enabledelayedexpansion

cd /d "%~dp0"

REM ── Python venv setup (optional) ─────────────────────────────────────────────
set VENV_DIR=.venv
set REQUIREMENTS=requirements.txt

if not exist "%VENV_DIR%\Scripts\python3.exe" (
    where uv >nul 2>nul
    if !ERRORLEVEL! equ 0 (
        echo [run.bat] Setting up Python venv with uv ...
        call uv venv "%VENV_DIR%" && call uv pip install -r "%REQUIREMENTS%"
        if !ERRORLEVEL! equ 0 ( echo [run.bat] Python venv ready. ) else ( echo [run.bat] Warning: Python venv setup failed )
    ) else (
        where python3 >nul 2>nul
        if !ERRORLEVEL! equ 0 (
            echo [run.bat] Setting up Python venv ...
            call python3 -m venv "%VENV_DIR%" && call "%VENV_DIR%\Scripts\pip" install -r "%REQUIREMENTS%"
            if !ERRORLEVEL! equ 0 ( echo [run.bat] Python venv ready. ) else ( echo [run.bat] Warning: Python venv setup failed )
        ) else (
            echo [run.bat] Warning: python3 not found -- Python MCP servers will be unavailable.
        )
    )
)

REM Prepend venv to PATH if available
if exist "%VENV_DIR%\Scripts\python3.exe" (
    set "PATH=%CD%\%VENV_DIR%\Scripts;%PATH%"
)

set "TS_RS_EXPERIMENT=this_is_unstable_software"

:loop
echo [run.bat] Starting ...
set RUSTFLAGS=-A warnings
cargo run
set code=!ERRORLEVEL!

if "!code!"=="0" (
    echo [run.bat] App exited cleanly. Stopping.
    exit /b 0
)
if "!code!"=="255" (
    echo [run.bat] App requested restart (exit -1). Rebuilding ...
    goto loop
)

echo [run.bat] App exited with code !code!. Stopping.
exit /b !code!
