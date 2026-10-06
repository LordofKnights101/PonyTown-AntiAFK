@echo off
rem Launcher for Pony Town Anti-AFK (no console window)
setlocal
set "PYW="
for /f "delims=" %%i in ('where pythonw 2^>nul') do if not defined PYW set "PYW=%%i"
if not defined PYW (
  for /f "delims=" %%i in ('where python 2^>nul') do (
    if not defined PYW if exist "%%~dpi\pythonw.exe" set "PYW=%%~dpi\pythonw.exe"
  )
)
if not defined PYW (
  echo Python 3 with tkinter is required to run this app.
  echo Install it from https://www.python.org/downloads/ ^(tick "Add to PATH"^).
  pause
  exit /b 1
)
start "Pony Town Anti-AFK" "%PYW%" "%~dp0app\main.pyw"
endlocal
