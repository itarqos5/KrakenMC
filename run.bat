@echo off
setlocal

set "PROJECT_DIR=%~dp0"
set "TESTS_DIR=%PROJECT_DIR%tests"
set "KRAKEN_EXE=%TESTS_DIR%\kraken.exe"

if not exist "%KRAKEN_EXE%" (
    echo kraken.exe was not found in tests. Building release executable...
    pushd "%PROJECT_DIR%" || exit /b 1
    cargo build --release
    if errorlevel 1 (
        popd
        echo Build failed.
        exit /b 1
    )
    popd

    if not exist "%TESTS_DIR%" mkdir "%TESTS_DIR%"
    copy /y "%PROJECT_DIR%target\release\kraken.exe" "%KRAKEN_EXE%" >nul
    if errorlevel 1 (
        echo Failed to copy kraken.exe into tests.
        exit /b 1
    )
)

> "%TESTS_DIR%\eula.txt" echo eula=true

pushd "%TESTS_DIR%" || exit /b 1
"%KRAKEN_EXE%"
set "KRAKEN_EXIT_CODE=%ERRORLEVEL%"
popd

exit /b %KRAKEN_EXIT_CODE%
