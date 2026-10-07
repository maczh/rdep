@echo off
REM ===========================================================================
REM  rdep Windows 一键安装（批处理包装，最终调用 install.ps1）
REM
REM  用法（在管理员 cmd 中）：
REM    install.bat service    "C:\path\rdep-service.exe"   [InstallDir] [ListenPort] [WebPort]
REM    install.bat forwarder  "C:\path\rdep-forwarder.exe" [InstallDir] [ListenPort] [WebPort]
REM    install.bat uninstall service
REM ===========================================================================
setlocal

set "COMP=%~1"
set "BIN=%~2"
set "INSTALL_DIR=%~3"
if "%INSTALL_DIR%"=="" set "INSTALL_DIR=C:\rdep"
set "LISTEN_PORT=%~4"
set "WEB_PORT=%~5"

if "%COMP%"=="" goto :usage
if /i "%COMP%"=="uninstall" goto :uninstall

if "%BIN%"=="" goto :usage
if not exist "%BIN%" (
    echo [ERROR] 找不到二进制: %BIN%
    exit /b 1
)

echo == 调用 install.ps1（组件=%COMP%）==
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" ^
    -Component %COMP% ^
    -Binary "%BIN%" ^
    -InstallDir "%INSTALL_DIR%" ^
    -ListenPort "%LISTEN_PORT%" ^
    -WebPort "%WEB_PORT%"
if errorlevel 1 (
    echo [ERROR] 安装失败。
    exit /b 1
)
echo == 完成 ==
exit /b 0

:uninstall
set "UN_COMP=%~2"
if "%UN_COMP%"=="" goto :usage
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0install.ps1" -Component %UN_COMP% -Uninstall
exit /b %errorlevel%

:usage
echo 用法:
echo   install.bat service    ^<rdep-service.exe^>   [InstallDir] [ListenPort] [WebPort]
echo   install.bat forwarder  ^<rdep-forwarder.exe^> [InstallDir] [ListenPort] [WebPort]
echo   install.bat uninstall  service^|forwarder
exit /b 1
