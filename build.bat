@echo off
setlocal enabledelayedexpansion

REM Build cargo
cargo build %*

if errorlevel 1 (
    echo Cargo build failed
    exit /b 1
)

if "%1"=="--release" (
    set PROFILE=release
) else (
    set PROFILE=debug
)

set ELF=target\thumbv8m.main-none-eabihf\%PROFILE%\stm32n6-testing

if not exist "%ELF%" (
    echo Failed to find executable
    exit /b 1
)

echo Found ELF:
echo %ELF%

set BIN=%ELF%.bin

rust-objcopy ^
    -O binary ^
    "%ELF%" ^
    "%BIN%"

if errorlevel 1 (
    echo objcopy failed
    exit /b 1
)

echo Binary generated:
echo %BIN%

set OUT=%ELF%-Signed.bin

REM Sign image
STM32_SigningTool_CLI.exe ^
    -bin "%BIN%" ^
    -nk ^
    -of 0x80000000 ^
    -t fsbl ^
    -o %OUT% ^
    -hv 2.3 ^
    -dump %OUT% ^
    -align ^
    -s

if errorlevel 1 (
    echo Signing failed
    exit /b 1
)

echo Signed:
echo %OUT%

echo Build complete