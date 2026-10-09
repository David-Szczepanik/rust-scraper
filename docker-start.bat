@echo off
TITLE POSTGRES (docker)
setlocal

REM Creates or starts the Postgres container for the scraper. Run the scraper itself
REM with 0-run.bat. User, password and port are taken from DATABASE_URL in .env, so
REM the scraper connects with the same values the container is created with.

REM --- Settings ---------------------------------------------------------
set DB_CONTAINER=scraper-db
set DB_VOLUME=scraper-pgdata
set PG_DB=pravnianalyza
REM ----------------------------------------------------------------------

cd /d "%~dp0"

docker version >nul 2>nul
if %ERRORLEVEL% neq 0 (
    echo [ERROR] Docker is not running or not installed.
    pause
    exit /b 1
)

if not exist .env (
    echo [ERROR] .env not found. Copy .env.example to .env and set DATABASE_URL.
    pause
    exit /b 1
)

REM DATABASE_URL=postgres://USER:PASSWORD@HOST:PORT/DB  ->  split on ':' and '@'
set PG_USER=
set PG_PASSWORD=
set PG_PORT=
for /f "tokens=1,2,3,4,5 delims=:@/" %%a in ('findstr /b "DATABASE_URL=" .env') do (
    set PG_USER=%%b
    set PG_PASSWORD=%%c
    set PG_PORT=%%e
)
if not defined PG_PASSWORD (
    echo [ERROR] Could not read user and password from DATABASE_URL in .env.
    echo         Expected: DATABASE_URL=postgres://user:password@localhost:5432/db
    pause
    exit /b 1
)
if not defined PG_PORT set PG_PORT=5432

docker volume inspect %DB_VOLUME% >nul 2>nul || docker volume create %DB_VOLUME% >nul

docker container inspect %DB_CONTAINER% >nul 2>nul
if %ERRORLEVEL% equ 0 (
    echo [INFO] Starting existing %DB_CONTAINER%
    docker start %DB_CONTAINER% >nul
) else (
    echo [INFO] Creating %DB_CONTAINER% - db\init.sql runs on this first start
    docker run -d --name %DB_CONTAINER% ^
        -e POSTGRES_USER=%PG_USER% -e POSTGRES_PASSWORD=%PG_PASSWORD% -e POSTGRES_DB=%PG_DB% ^
        -v %DB_VOLUME%:/var/lib/postgresql/data ^
        -v "%CD%\db\init.sql:/docker-entrypoint-initdb.d/init.sql:ro" ^
        -p %PG_PORT%:5432 ^
        postgres:16-alpine >nul
    if %ERRORLEVEL% neq 0 (
        echo [ERROR] Could not start Postgres. Is port %PG_PORT% already in use?
        pause
        exit /b 1
    )
)

REM Probe over TCP: during first-run init the temporary server only listens on the socket.
echo [INFO] Waiting for Postgres...
set /a TRIES=0
:wait_db
docker exec %DB_CONTAINER% pg_isready -h 127.0.0.1 -U %PG_USER% -d %PG_DB% >nul 2>nul
if %ERRORLEVEL% equ 0 goto db_ready
set /a TRIES+=1
if %TRIES% geq 30 (
    echo [ERROR] Postgres did not become ready. Check: docker logs %DB_CONTAINER%
    pause
    exit /b 1
)
timeout /t 1 /nobreak >nul
goto wait_db
:db_ready

echo.
echo [SUCCESS] Postgres is ready on localhost:%PG_PORT%  (user %PG_USER%, db %PG_DB%)
echo           Now run 0-run.bat
pause
