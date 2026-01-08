# Build and Push script for Rust Scraper
# Requires Docker Desktop with Buildx enabled
# Requires being logged in: docker login ghcr.io

$Registry = "ghcr.io"
$ImageName = "david-szczepanik/rust-scraper" # Update if repo name changes
$FullImageName = "$Registry/$ImageName"
$GitHash = $(git rev-parse --short HEAD)
$TagLatest = $FullImageName + ":latest"
$TagVersion = $FullImageName + ":" + $GitHash

Write-Host "🚀 Starting local build and push for $FullImageName" -ForegroundColor Cyan

# Ensure buildx builder exists and is selected
docker buildx create --use --name mybuilder --node mybuilder0 --driver docker-container --bootstrap 2>$null
docker buildx use mybuilder

Write-Host "🏗️ Building and pushing ARM64 image (cax11 optimized)..." -ForegroundColor Yellow

$ErrorActionPreference = "Stop"

docker buildx build `
    --platform linux/arm64 `
    --tag $TagLatest `
    --tag $TagVersion `
    --push `
    .

if ($LASTEXITCODE -ne 0) {
    Write-Host "âťŚ Build failed with exit code $LASTEXITCODE" -ForegroundColor Red
    Read-Host "Press Enter to exit..."
    exit $LASTEXITCODE
}

Write-Host "✅ Done! Image pushed to $FullImageName" -ForegroundColor Green

Read-Host "Press Enter to exit..."
