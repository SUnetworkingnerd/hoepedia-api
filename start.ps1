# Start Hoepedia on Windows with whatever is available on this machine:
#   1. hoepedia-api.exe next to this script (a downloaded release)
#   2. Docker Desktop
#   3. Rust (builds the program the first time; takes a few minutes)
# No configuration is needed. Options are passed to the server when it runs directly
# (1 and 3), for example:   .\start.ps1 --port 8080
# Use   .\start.ps1 --native   to skip Docker.
$ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

$native = $false
$rest = @()
foreach ($a in $args) {
    if ($a -eq '--native') { $native = $true } else { $rest += $a }
}

function Test-Command($name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

if (Test-Path .\hoepedia-api.exe) {
    & .\hoepedia-api.exe @rest
    exit $LASTEXITCODE
}

if (-not $native -and (Test-Command docker)) {
    docker compose version *> $null
    if ($LASTEXITCODE -eq 0) {
        docker info *> $null
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'Docker is installed but not running. Start Docker Desktop and run this again, or use: .\start.ps1 --native'
            exit 1
        }
        Write-Host 'Starting Hoepedia with Docker (the first start builds the image and takes a few minutes)...'
        docker compose up -d --build
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        $port = '3000'
        if (Test-Path .env) {
            $line = Get-Content .env | Where-Object { $_ -match '^HOST_PORT=' } | Select-Object -Last 1
            if ($line) { $port = ($line -replace '^HOST_PORT=', '').Trim().Trim('"') }
        }
        $ready = $false
        for ($i = 0; $i -lt 60; $i++) {
            docker compose exec -T hoepedia-api hoepedia-api --healthcheck *> $null
            if ($LASTEXITCODE -eq 0) { $ready = $true; break }
            Start-Sleep -Seconds 1
        }
        if (-not $ready) {
            Write-Host 'The server did not become ready. Look at: docker compose logs hoepedia-api'
            exit 1
        }
        $token = (docker compose exec -T hoepedia-api hoepedia-api token 2> $null | Out-String).Trim()
        Write-Host ''
        Write-Host "  Hoepedia is running:  http://localhost:$port"
        Write-Host "  Admin token:          $token"
        Write-Host ''
        Write-Host '  Test it:   docker compose exec hoepedia-api hoepedia-api check'
        Write-Host '  Logs:      docker compose logs -f hoepedia-api'
        Write-Host '  Stop:      docker compose down        (your data stays in the Docker volume)'
        exit 0
    }
}

if (Test-Command cargo) {
    $exe = '.\target\release\hoepedia-api.exe'
    $build = -not (Test-Path $exe)
    if (-not $build) {
        $built = (Get-Item $exe).LastWriteTime
        $newer = Get-ChildItem -Recurse -File src, Cargo.toml, Cargo.lock | Where-Object { $_.LastWriteTime -gt $built } | Select-Object -First 1
        if ($newer) { $build = $true }
    }
    if ($build) {
        Write-Host 'Building Hoepedia (the first build takes a few minutes)...'
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    & $exe @rest
    exit $LASTEXITCODE
}

Write-Host @'
Hoepedia needs one of these to run:

  * Docker Desktop   https://docs.docker.com/desktop/install/windows-install/   (easiest)
  * Rust             https://rustup.rs   (builds it on this machine; the installer also
                     offers to install the Visual Studio C++ Build Tools, which are needed)

Install one of them and run start.bat again.
'@
exit 1
