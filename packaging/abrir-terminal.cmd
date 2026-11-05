@echo off
chcp 65001 >nul
cd /d "%~dp0"
cmd /k "spotify-sync.exe --help"
