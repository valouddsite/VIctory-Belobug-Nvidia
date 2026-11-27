@echo off
setlocal
cd /d "%~dp0"
where powershell.exe >nul 2>nul
if errorlevel 1 (
  echo Windows PowerShell is unavailable.
  pause
  exit /b 1
)
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\launcher.ps1"
set "RESULT=%ERRORLEVEL%"
echo.
if "%RESULT%"=="0" (
  echo VIctory launcher finished.
) else (
  echo VIctory launcher exited with code %RESULT%.
)
pause
exit /b %RESULT%
