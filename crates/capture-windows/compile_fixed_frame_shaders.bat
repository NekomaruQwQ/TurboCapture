@echo off
setlocal

rem The just recipe supplies a Nushell login environment in which fxc.exe is on PATH.
set "SHADER_SOURCE=%~dp0src\fixed_frame.hlsl"
set "SHADER_OUTPUT_DIRECTORY=%~dp0generated"

if not exist "%SHADER_OUTPUT_DIRECTORY%" mkdir "%SHADER_OUTPUT_DIRECTORY%"
if not exist "%SHADER_OUTPUT_DIRECTORY%" (
    >&2 echo Failed to create shader output directory: %SHADER_OUTPUT_DIRECTORY%
    exit /b 1
)

fxc.exe /nologo /O3 /Zi /WX /T vs_5_0 /E vs_main /Fo "%SHADER_OUTPUT_DIRECTORY%\fixed_frame_vs_main.fxo" "%SHADER_SOURCE%"
if errorlevel 1 exit /b %errorlevel%

fxc.exe /nologo /O3 /Zi /WX /T ps_5_0 /E ps_main /Fo "%SHADER_OUTPUT_DIRECTORY%\fixed_frame_ps_main.fxo" "%SHADER_SOURCE%"
if errorlevel 1 exit /b %errorlevel%

exit /b 0
