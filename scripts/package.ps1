﻿# pad
# DyPlayer Windows x64 portable package builder（双版本：wgpu 主流 + glow 老设备）
# Usage: powershell -ExecutionPolicy Bypass -File scripts/package.ps1
# Output:
#   dist/dyplayer-<ver>-win-x64/ + .zip         (wgpu/D3D12，主流设备)
#   dist/dyplayer-<ver>-win-x64-legacy/ + .zip  (glow/OpenGL，*老设备 SW-only)
#
# Bundled runtime deps:
#   libmpv-2.dll   - from vendor/mpv-dev (exe-relative, see MpvLibrary::load)
#   vulkan-1.dll   - statically imported by libmpv-2.dll; copied from System32.
#                    It is only the Vulkan loader: safe to bundle, and mpv falls
#                    back to other renderers when no Vulkan ICD exists.
#   fonts/         - HarmonyOS Sans SC (UI font, exe-relative lookup)
# Danmaku font is loaded from system font candidates; no bundling needed.

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

$version = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $root "dist"

function Stage-Package($pkgName) {
    $pkg = Join-Path $dist $pkgName
    $exe = Join-Path $root "target\release\dyplayer.exe"
    if (-not (Test-Path $exe)) { throw "dyplayer.exe not found at $exe" }

    Write-Host "==> staging $pkg"
    Remove-Item $pkg -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Path $pkg -Force | Out-Null

    Copy-Item $exe $pkg
    Copy-Item (Join-Path $root "vendor\mpv-dev\libmpv-2.dll") $pkg

    $vulkan = Join-Path $env:SystemRoot "System32\vulkan-1.dll"
    if (Test-Path $vulkan) {
        Copy-Item $vulkan $pkg
    } else {
        Write-Warning "vulkan-1.dll not found in System32; target machines need the Vulkan runtime installed"
    }

    Copy-Item (Join-Path $root "fonts") (Join-Path $pkg "fonts") -Recurse

    $zip = Join-Path $dist "$pkgName.zip"
    Write-Host "==> compressing $zip"
    Compress-Archive -Path (Join-Path $pkg "*") -DestinationPath $zip -Force
}

# --- 主版本（wgpu/D3D12，主流设备，含 wid GPU 路径 + 覆盖层）---
Write-Host "==> cargo build --release -p dyplayer-app (wgpu)"
Push-Location $root
cargo build --release -p dyplayer-app
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (wgpu)" }
Pop-Location
Stage-Package "dyplayer-$version-win-x64"

# --- 老设备版本（glow/OpenGL，SW-only，Win7+ + DX9 级硬件）---
Write-Host "==> cargo build --release -p dyplayer-app --no-default-features --features backend-glow (glow)"
Push-Location $root
cargo build --release -p dyplayer-app --no-default-features --features backend-glow
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (glow)" }
Pop-Location
Stage-Package "dyplayer-$version-win-x64-legacy"

Write-Host "==> done"
Get-ChildItem $dist | Select-Object Name, @{N = "SizeMB"; E = { [math]::Round($_.Length / 1MB, 1) } }