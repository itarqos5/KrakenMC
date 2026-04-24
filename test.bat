@echo off
echo Building kraken optimally...
cargo build --release
if %errorlevel% neq 0 exit /b %errorlevel%

echo Cleaning tests directory...
if exist tests\ (
    rmdir /s /q tests
)
mkdir tests

echo Copying executable...
copy target\release\kraken.exe tests\ >nul

echo Starting server...
cd tests
kraken.exe
cd ..
