@echo off
rem Double-click this file (or run it from a terminal) to start Hoepedia on Windows.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start.ps1" %*
if errorlevel 1 pause
