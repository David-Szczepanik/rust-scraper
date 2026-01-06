@echo off
TITLE RUST-SCRAPER
echo Starting Rust Scraper...

REM Set environment if needed
REM set RUST_LOG=info

REM Check if cargo is installed
where cargo >nul 2>nul
if %ERRORLEVEL% neq 0 (
    echo [ERROR] cargo is not installed or not in PATH.
    echo Please install Rust from https://rustup.rs/
    pause
    exit /b 1
)

echo [INFO] Executing: cargo run
cargo run

if %ERRORLEVEL% neq 0 (
    echo.
    echo [ERROR] Rust Scraper stopped with error code %ERRORLEVEL%.
    pause
) else (
    echo.
    echo [SUCCESS] Rust Scraper stopped.
    pause
)
