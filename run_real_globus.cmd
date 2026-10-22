@echo off
cd /d "%~dp0"
set GLOBUS_CLIENT_ID=2cc0a4ef-235b-4f90-b754-843df1e3c021
rem GLOBUS_CLIENT_SECRET comes from the environment; it is never stored here.
if not defined GLOBUS_CLIENT_SECRET (
  echo GLOBUS_CLIENT_SECRET is not set. Set it first, e.g. in PowerShell: $env:GLOBUS_CLIENT_SECRET = "..."
  pause
  exit /b 1
)
target\release\idp_redirect_demo.exe
pause
