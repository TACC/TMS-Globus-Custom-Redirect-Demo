@echo off
cd /d "%~dp0"
set GLOBUS_CLIENT_ID=2cc0a4ef-235b-4f90-b754-843df1e3c021
target\release\idp_redirect_demo.exe
pause
