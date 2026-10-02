# Gets the pinned third-party files the build needs into third_party, which
# git ignores. Each part checks what is already there and does its work only
# when its files are missing or do not match the pin.
#
# FFmpeg: the LGPL DLLs the viewer decodes with, built from FFmpeg's source by
# tools\build-ffmpeg.ps1. It is called from here, not only named, so a release
# stays one command from a fresh clone; when third_party\ffmpeg already holds
# the pinned build it says so and does nothing. The first time it takes a few
# minutes and needs the Visual Studio 2022 Build Tools.
#
# nv-codec-headers: nvEncodeAPI.h, which crates/encode compiles its NVENC
# layout test against. MIT, the notice is in the file.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot

function Get-Sha256($path) {
    (Get-FileHash $path -Algorithm SHA256).Hash
}

# --- FFmpeg ---------------------------------------------------------------

& (Join-Path $PSScriptRoot 'build-ffmpeg.ps1')

# --- nv-codec-headers -----------------------------------------------------

$nvencUrl = 'https://raw.githubusercontent.com/FFmpeg/nv-codec-headers/n12.2.72.0/include/ffnvcodec/nvEncodeAPI.h'
$nvencSha256 = '4677A397E3EC5300A6B38BF49CBA42BB63A922AB26F24BC63A05ED08857CBA16'
$nvencTarget = Join-Path $root 'third_party\nv-codec-headers'
$nvencHeader = Join-Path $nvencTarget 'nvEncodeAPI.h'

if ((Test-Path $nvencHeader) -and (Get-Sha256 $nvencHeader) -eq $nvencSha256) {
    Write-Host "nvEncodeAPI.h is already in $nvencTarget (n12.2.72.0), nothing to fetch"
} else {
    if (Test-Path $nvencHeader) {
        Write-Host "$nvencHeader does not match the pinned hash, fetching it again"
    }
    $download = Join-Path $env:TEMP 'nvEncodeAPI-n12.2.72.0.h'
    Write-Host "downloading $nvencUrl"
    Invoke-WebRequest -Uri $nvencUrl -OutFile $download -UseBasicParsing
    $got = Get-Sha256 $download
    if ($got -ne $nvencSha256) {
        Remove-Item -Force $download
        throw "the nvEncodeAPI.h download does not match the pinned hash: expected $nvencSha256, got $got. Nothing was changed in $nvencTarget."
    }
    New-Item -ItemType Directory -Force $nvencTarget | Out-Null
    Move-Item -Force $download $nvencHeader
    Write-Host "nvEncodeAPI.h is in $nvencTarget"
}
