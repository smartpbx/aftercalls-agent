$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$ffmpegVersion = "n7.1.5-12-g1fdbca85aa-20260731"
$ffmpegUrl = "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-win64-lgpl-7.1.zip"
$ffmpegSha256 = "b7c1c846dacca68ee4ebf5c390742c973b3d5d14a6d44b061f500d8e4ac74fc0"
$agentDir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$binariesDir = Join-Path $agentDir "src-tauri/binaries"
$resourcesDir = Join-Path $agentDir "src-tauri/resources"
$stageDir = Join-Path $env:RUNNER_TEMP ("aftercalls-ffmpeg-" + [guid]::NewGuid())
$archive = Join-Path $stageDir "ffmpeg.zip"
$unpackDir = Join-Path $stageDir "unpacked"
$destination = Join-Path $binariesDir "ffmpeg-aftercalls-x86_64-pc-windows-msvc.exe"
$noticesSource = Join-Path $agentDir "THIRD_PARTY_NOTICES.md"
if (-not (Test-Path $noticesSource)) {
    $noticesSource = Join-Path (Split-Path $agentDir -Parent) "THIRD_PARTY_NOTICES.md"
}
if (-not (Test-Path $noticesSource)) {
    throw "THIRD_PARTY_NOTICES.md is missing from the checkout"
}

try {
    New-Item -ItemType Directory -Force -Path $binariesDir, $resourcesDir, $unpackDir | Out-Null

    $downloaded = $false
    foreach ($attempt in 1..3) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $ffmpegUrl -OutFile $archive
            $downloaded = $true
            break
        }
        catch {
            if ($attempt -eq 3) { throw }
            Start-Sleep -Seconds ([Math]::Pow(2, $attempt))
        }
    }
    if (-not $downloaded) { throw "ffmpeg download did not complete" }

    $actualSha256 = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
    if ($actualSha256 -ne $ffmpegSha256) {
        throw "ffmpeg sha256 mismatch: expected $ffmpegSha256, got $actualSha256"
    }

    Expand-Archive -Path $archive -DestinationPath $unpackDir -Force
    $ffmpeg = Get-ChildItem -Path $unpackDir -Filter "ffmpeg.exe" -Recurse |
        Select-Object -First 1
    $license = Get-ChildItem -Path $unpackDir -Filter "LICENSE.txt" -Recurse |
        Select-Object -First 1
    if (-not $ffmpeg) {
        throw "downloaded ffmpeg archive did not contain ffmpeg.exe"
    }
    if (-not $license) {
        throw "downloaded ffmpeg archive did not contain LICENSE.txt"
    }

    Copy-Item $ffmpeg.FullName $destination
    $licenseDestination = Join-Path $resourcesDir "FFmpeg-LICENSE.txt"
    $noticesDestination = Join-Path $resourcesDir "THIRD_PARTY_NOTICES.md"
    Copy-Item $license.FullName $licenseDestination
    Copy-Item $noticesSource $noticesDestination

    $versionOutput = (& $destination -hide_banner -version 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg -version failed" }
    Write-Output $versionOutput
    if (-not $versionOutput.Contains("ffmpeg version $ffmpegVersion")) {
        throw "unexpected ffmpeg build identity"
    }
    if ($versionOutput -match '--enable-(gpl|nonfree)') {
        throw "refusing a GPL or non-redistributable ffmpeg build"
    }
    if (-not $versionOutput.Contains("--enable-libopus")) {
        throw "ffmpeg build is missing the required libopus encoder"
    }
    if (-not (Select-String -SimpleMatch "GNU LESSER GENERAL PUBLIC LICENSE" -Path $licenseDestination)) {
        throw "ffmpeg archive did not contain the expected LGPL license"
    }

    Get-FileHash -Algorithm SHA256 $destination, $licenseDestination, $noticesDestination |
        Format-Table -AutoSize Algorithm, Hash, Path
}
finally {
    if (Test-Path $stageDir) {
        Remove-Item -Recurse -Force $stageDir
    }
}
