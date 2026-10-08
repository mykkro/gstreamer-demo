@echo off
rem Run before building/running the Rust demo in cmd.exe:   env.cmd
rem Points cargo (pkg-config) and the binaries (DLLs) at the GStreamer MSVC SDK, for this window only.
set "GSTROOT=%GSTREAMER_1_0_ROOT_MSVC_X86_64%"
if defined GSTROOT if not exist "%GSTROOT%\bin\pkg-config.exe" set "GSTROOT="
if not defined GSTROOT if exist "%LOCALAPPDATA%\Programs\gstreamer\1.0\msvc_x86_64\bin\pkg-config.exe" set "GSTROOT=%LOCALAPPDATA%\Programs\gstreamer\1.0\msvc_x86_64"
if not defined GSTROOT if exist "%ProgramFiles%\gstreamer\1.0\msvc_x86_64\bin\pkg-config.exe" set "GSTROOT=%ProgramFiles%\gstreamer\1.0\msvc_x86_64"
if not defined GSTROOT if exist "C:\gstreamer\1.0\msvc_x86_64\bin\pkg-config.exe" set "GSTROOT=C:\gstreamer\1.0\msvc_x86_64"
if not defined GSTROOT (
    echo GStreamer MSVC SDK ^(devel^) not found. Install it from https://gstreamer.freedesktop.org/download/ with the 'devel' type.
    exit /b 1
)
if "%GSTROOT:~-1%"=="\" set "GSTROOT=%GSTROOT:~0,-1%"
set "GSTREAMER_1_0_ROOT_MSVC_X86_64=%GSTROOT%\"
set "PKG_CONFIG=%GSTROOT%\bin\pkg-config.exe"
set "PKG_CONFIG_PATH=%GSTROOT%\lib\pkgconfig"
echo ;%PATH%; | find /i ";%GSTROOT%\bin;" >nul || set "PATH=%GSTROOT%\bin;%PATH%"
echo GStreamer SDK: %GSTROOT%
set "GSTROOT="
