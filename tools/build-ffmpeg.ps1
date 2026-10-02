# Builds the FFmpeg DLLs Booth decodes video with from FFmpeg's own source
# and puts them in third_party\ffmpeg:
#
#   powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1
#
# It needs Windows 11, whose tar.exe unpacks the .tar.xz and .tar.zst
# downloads, and the Visual Studio 2022 Build Tools with the C++ x64
# compiler. Everything else it downloads once into a folder of its own
# outside the repository, %LOCALAPPDATA%\booth-ffmpeg-build unless -Work
# names another, and checks against the SHA-256 pins below on every run:
# FFmpeg's source archive from ffmpeg.org, and MSYS2's base archive and
# three packages for the shell, make and nasm that FFmpeg's build system
# needs. Nothing is installed and nothing asks for administrator rights;
# deleting that folder removes all of it. Its path must have no spaces,
# because the compiler and make split paths at them.
#
# The pins were taken after checking each download the way its publisher
# says to: the source archive's signature against FFmpeg's release signing
# key, FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8 as ffmpeg.org
# gives it; MSYS2's base archive against the SHA-256 its release page
# publishes; each package's signature against the MSYS2 keyring in that
# base archive.
#
# The DLLs are pinned too: the same source, recipe, compiler and Windows
# SDK give the same bytes, and a build that gives other bytes stops before
# third_party\ffmpeg is touched. When third_party\ffmpeg already holds the
# pinned build there is nothing to do, unless -Force asks for the build to be
# made again, which shows whether this PC still gives the pinned bytes.
#
# The configure line and the reasons for each option are in
# tools\ffmpeg\build.sh. The build happens in a scratch folder and
# third_party\ffmpeg is swapped in at the end with two renames, so a cargo
# build running at the same time never sees it half written.
#
# SOURCE.txt, written last, names the build, its source and the recipe, and
# the release of Booth's that holds the source archive and the recipe, which
# tools\release.ps1 -FfmpegSource makes. A SOURCE.txt that names another
# release, such as one from before the address changed, counts as a folder
# that needs building again.
param(
    [string]$Work = (Join-Path $env:LOCALAPPDATA 'booth-ffmpeg-build'),
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = Split-Path -Parent $PSScriptRoot

$version = '8.1.3'
$sourceUrl = "https://ffmpeg.org/releases/ffmpeg-$version.tar.xz"
$sourceSha256 = '7138D28C96D9D3E3AF4EE3D8CAD72741F8FFB40DA90C1112235DEA3ECD3178A3'

# The release SOURCE.txt names as the home of the source archive and this
# recipe. Its address is written here rather than read from the app's code
# so that the recipe zip in that release runs on its own; tools\release.ps1
# refuses a SOURCE.txt whose address is not under the app's RELEASES_PAGE.
# A changed build.sh with the same FFmpeg needs a release under a new name,
# because the one already up keeps the recipe it was made with.
$releasesPage = 'https://github.com/Shadi-Alrashoodi/booth/releases'
$sourceRelease = "ffmpeg-$version"
$recipeLine = "Recipe: tools\build-ffmpeg.ps1 and tools\ffmpeg\build.sh from Booth's repository as ffmpeg-$version-recipe.zip, in the release $sourceRelease at $releasesPage/tag/$sourceRelease with the exact source archive, which is also on ffmpeg.org at the Source address"

$msys2Url = 'https://github.com/msys2/msys2-installer/releases/download/2026-09-27/msys2-base-x86_64-20260927.tar.xz'
$msys2Sha256 = 'EA2F31A0B6ADE63914CE441FFB022F0F6AA96982BFEFA2326460A26D5FB01322'
$packagesUrl = 'https://repo.msys2.org/msys/x86_64'
$packages = [ordered]@{
    'make-4.4.1-3-x86_64.pkg.tar.zst'    = 'AF0BDBA17F06FE037F0194069ADAA31A8FE45F1A11381501896AEA1FAE37BD5D'
    'diffutils-3.12-1-x86_64.pkg.tar.zst' = '7902C8CE3D4DD69A0F5E98DC9D5C83C17B23314BA486169DB57EF6E2835CE3B6'
    'nasm-3.02-1-x86_64.pkg.tar.zst'     = '1AB3D9D7F86F57A66CC506799648527B9215C3EB1B7BD404D886D59B503A05B7'
}

# What this PC made from the pins above with this MSVC and Windows SDK.
# -MT compiles the C runtime into each DLL, part of it from the compiler and
# part from the SDK (libucrt.lib), so another version of either makes other
# bytes, and the error says so.
$msvc = '14.44.35207'
$sdk = '10.0.26100.0'
$dllPins = [ordered]@{
    'avcodec-62.dll' = 'A4F0D590B6315CF00F515E1ECDC5167604E5465C0797F65F33E99B3C1C9CD89E'
    'avutil-60.dll'  = '8D823875FB6CE98483F4E7BBB347F3392D26F4AE88BD7F181C7F2C7A2ABC4B1D'
}

$target = Join-Path $root 'third_party\ffmpeg'
$recipe = Join-Path $PSScriptRoot 'ffmpeg\build.sh'
$archiveName = "ffmpeg-$version.tar.xz"
$tar = Join-Path $env:SystemRoot 'System32\tar.exe'
$command = 'powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1'
# A relative -Work means the folder PowerShell is in, which .NET calls and
# the processes started below do not follow, so it is made absolute here.
$Work = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Work)

function Get-Sha256([string]$Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
}

# $null when the folder holds the pinned build, or what is wrong with it.
function Test-FFmpegFolder {
    if (-not (Test-Path -LiteralPath $target)) { return "$target does not exist" }
    foreach ($dll in $dllPins.Keys) {
        $path = Join-Path $target "bin\$dll"
        if (-not (Test-Path -LiteralPath $path)) { return "$path is missing" }
        if ((Get-Sha256 $path) -ne $dllPins[$dll]) { return "$path is not the pinned build" }
    }
    $others = @(Get-ChildItem -LiteralPath (Join-Path $target 'bin') -Filter *.dll | Where-Object { -not $dllPins.Contains($_.Name) })
    if ($others) { return "$target\bin has DLLs the pinned build does not make: $($others.Name -join ', ')" }
    foreach ($file in 'include\libavcodec\avcodec.h', 'include\libavutil\hwcontext_d3d11va.h', 'LICENSE.txt', 'NOTICES.txt', 'SOURCE.txt', $archiveName) {
        if (-not (Test-Path -LiteralPath (Join-Path $target $file))) { return "$target\$file is missing" }
    }
    if ((Get-Sha256 (Join-Path $target $archiveName)) -ne $sourceSha256) { return "$target\$archiveName is not the pinned source archive" }
    $source = Get-Content -LiteralPath (Join-Path $target 'SOURCE.txt')
    if ($source -notcontains "build.sh SHA-256: $(Get-Sha256 $recipe)") { return "tools\ffmpeg\build.sh has changed since $target was built" }
    if ($source -cnotcontains $recipeLine) { return "$target\SOURCE.txt does not name the release $sourceRelease at $releasesPage/tag/$sourceRelease as the place of the source and the recipe" }
    return $null
}

# A file in the downloads folder is used again only while it matches its pin.
function Get-Pinned([string]$Url, [string]$Sha256) {
    $file = Join-Path $downloads ($Url -replace '^.*/', '')
    if ((Test-Path -LiteralPath $file) -and (Get-Sha256 $file) -eq $Sha256) { return $file }
    $part = "$file.part"
    Write-Host "downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $part -UseBasicParsing
    $got = Get-Sha256 $part
    if ($got -ne $Sha256) {
        Remove-Item -LiteralPath $part -Force
        throw "the download of $Url does not match its pin: expected $Sha256, got $got. Nothing was built."
    }
    Move-Item -LiteralPath $part -Destination $file -Force
    return $file
}

function Invoke-Tar([string]$Archive, [string]$Into, [string[]]$More) {
    & $tar -xf $Archive -C $Into @More
    if ($LASTEXITCODE -ne 0) { throw "could not unpack $Archive into $Into (tar exit code $LASTEXITCODE)" }
}

# Rename, not copy, so the swap takes no time; a folder another program has
# a file open in cannot be renamed, so it is tried for a few seconds.
function Move-Folder([string]$From, [string]$To) {
    for ($try = 1; ; $try++) {
        try {
            [System.IO.Directory]::Move($From, $To)
            return
        } catch {
            if ($try -ge 20) {
                $reason = if ($_.Exception.InnerException) { $_.Exception.InnerException.Message } else { $_.Exception.Message }
                throw "could not rename $From to ${To}: $reason Close what has a file open in it and run $command again."
            }
            Start-Sleep -Milliseconds 250
        }
    }
}

$problem = Test-FFmpegFolder
if (-not $problem -and -not $Force) {
    Write-Host "$target already holds the pinned FFmpeg $version build, nothing to do"
    return
}
if ($problem) { Write-Host "$problem; building FFmpeg $version" } else { Write-Host "building FFmpeg $version again (-Force)" }
$started = Get-Date

if ($Work -match '\s') {
    throw "the build folder $Work has a space in its path, and FFmpeg's build splits paths at spaces, so it would build DLLs that differ from the pins. Name a folder without spaces, for example: $command -Work C:\booth-ffmpeg-build"
}

# libarchive names the compression libraries it was built with in its
# version line. Without these two, tar.exe cannot unpack the downloads.
$tarSays = if (Test-Path -LiteralPath $tar) { "it says: $((& $tar --version) -join ' ')" } else { "$tar does not exist" }
if ($tarSays -notmatch 'liblzma/' -or $tarSays -notmatch 'libzstd/') {
    throw "the downloads are .tar.xz and .tar.zst files, which need Windows' tar.exe built with liblzma and libzstd, as Windows 11's is; $tarSays. Run $command on Windows 11."
}

# --- Visual Studio --------------------------------------------------------

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
function Get-VisualStudio([string]$Property) {
    & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property $Property | Select-Object -First 1
}
$vs = if (Test-Path -LiteralPath $vswhere) { Get-VisualStudio installationPath }
$vcvars = if ($vs) { Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat' }
if (-not $vcvars -or -not (Test-Path -LiteralPath $vcvars)) {
    throw "could not find the Visual Studio C++ x64 build tools (vcvars64.bat). Install the Visual Studio 2022 Build Tools with `"Desktop development with C++`" and run $command again."
}
$product = '{0} {1}' -f (Get-VisualStudio displayName), ((Get-VisualStudio catalog_productDisplayVersion) -replace ' .*$', '')

# The x64 compiler's environment from vcvars64.bat, handed to MSYS2's bash
# so configure finds cl, link, lib and the Windows SDK. $null when
# vcvars64.bat fails, as it does when asked for versions that are not
# installed.
function Get-VcEnvironment([string]$Versions) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = Join-Path $env:SystemRoot 'System32\cmd.exe'
    $info.Arguments = "/d /s /c `"`"$vcvars`" $Versions >nul 2>&1 && set`""
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $cmd = [System.Diagnostics.Process]::Start($info)
    $lines = $cmd.StandardOutput.ReadToEnd() -split "`r?`n" | Where-Object { $_ -match '^[^=]+=' }
    $cmd.WaitForExit()
    if ($cmd.ExitCode -ne 0 -or -not ($lines -match '^VCToolsVersion=')) { return $null }
    return $lines
}

# Asked for by version, the pinned MSVC and SDK are used even when newer
# ones are installed beside them. When they are not there, the build goes
# on with what vcvars64.bat picks, so its hashes can become the new pins.
$environment = Get-VcEnvironment "$sdk -vcvars_ver=$msvc"
if (-not $environment) {
    $environment = Get-VcEnvironment ''
    if (-not $environment) {
        throw "$vcvars failed, so the compiler's environment could not be set up. Repair the Visual Studio 2022 Build Tools in the Visual Studio Installer and run $command again."
    }
}
function Get-Setting([string]$Name) {
    $line = $environment -match "^$Name=" | Select-Object -First 1
    if ($line) { $line.Substring($Name.Length + 1).TrimEnd('\') } else { 'unknown' }
}
$foundSdk = Get-Setting 'WindowsSDKVersion'
$foundUcrt = Get-Setting 'UCRTVersion'
$found = "MSVC $(Get-Setting 'VCToolsVersion') and Windows SDK $foundSdk"
if ($foundUcrt -ne $foundSdk) { $found += " with the C runtime of SDK $foundUcrt" }
$pinned = "MSVC $msvc and Windows SDK $sdk"
if ($found -ne $pinned) {
    Write-Warning "this PC builds with $found from $product, and the DLL pins are from $pinned, so the DLLs will most likely differ from the pins"
}

# --- Downloads and MSYS2 --------------------------------------------------

$downloads = Join-Path $Work 'downloads'
New-Item -ItemType Directory -Force $downloads | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$archive = Get-Pinned $sourceUrl $sourceSha256
$msys2Archive = Get-Pinned $msys2Url $msys2Sha256
$packageFiles = foreach ($name in $packages.Keys) { Get-Pinned "$packagesUrl/$name" $packages[$name] }

# MSYS2 is unpacked once and used again while the note in it names the same
# pins. The packages are unpacked over it rather than installed with pacman,
# which would first make a keyring of its own; their files are all they
# bring, and their hashes were checked above.
$msys2 = Join-Path $Work 'msys64'
$note = Join-Path $msys2 'booth-build-tools.txt'
$tools = @("$msys2Url $msys2Sha256") + ($packages.Keys | ForEach-Object { "$packagesUrl/$_ $($packages[$_])" })
if (-not (Test-Path -LiteralPath $note) -or (Compare-Object (Get-Content -LiteralPath $note) $tools)) {
    if (Test-Path -LiteralPath $msys2) {
        Write-Host "removing $msys2, which is not the pinned MSYS2"
        Remove-Item -LiteralPath $msys2 -Recurse -Force
    }
    Write-Host "unpacking MSYS2 into $msys2"
    Invoke-Tar $msys2Archive $Work @()
    foreach ($file in $packageFiles) {
        Invoke-Tar $file $msys2 @('--exclude', '.PKGINFO', '--exclude', '.MTREE', '--exclude', '.BUILDINFO', 'usr')
    }
    Set-Content -LiteralPath $note -Value $tools -Encoding ascii
}
$bash = Join-Path $msys2 'usr\bin\bash.exe'

# --- The build ------------------------------------------------------------

$build = Join-Path $Work 'build'
if (Test-Path -LiteralPath $build) { Remove-Item -LiteralPath $build -Recurse -Force }
New-Item -ItemType Directory -Force $build | Out-Null
Invoke-Tar $archive $build @()
$sourceDir = Join-Path $build "ffmpeg-$version"
$install = Join-Path $build 'install'
$log = Join-Path $Work 'build.log'

Write-Host "configuring and compiling FFmpeg $version with $found; the log is $log"
$info = New-Object System.Diagnostics.ProcessStartInfo
$info.FileName = $bash
$info.Arguments = (@('--noprofile', '--norc', $recipe, $sourceDir, $install, $log) | ForEach-Object { '"' + ($_ -replace '\\', '/') + '"' }) -join ' '
$info.UseShellExecute = $false
$info.EnvironmentVariables.Clear()
foreach ($line in $environment) {
    $at = $line.IndexOf('=')
    $info.EnvironmentVariables[$line.Substring(0, $at)] = $line.Substring($at + 1)
}
$process = [System.Diagnostics.Process]::Start($info)
$process.WaitForExit()
if ($process.ExitCode -ne 0) {
    if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log -Tail 40 | Write-Host }
    throw "the FFmpeg build failed with exit code $($process.ExitCode); the whole log is $log. Nothing in $target was changed."
}

$made = Join-Path $install 'ffmpeg'
$hashes = [ordered]@{}
$differs = @()
foreach ($dll in $dllPins.Keys) {
    $path = Join-Path $made "bin\$dll"
    if (-not (Test-Path -LiteralPath $path)) { throw "the build made no $dll; the log is $log" }
    $hashes[$dll] = Get-Sha256 $path
    if ($hashes[$dll] -ne $dllPins[$dll]) { $differs += "$dll is $($hashes[$dll]), pinned $($dllPins[$dll])" }
}
if ($differs) {
    $why = if ($found -ne $pinned) {
        "They were built with $found and the pins are from $pinned. If $found is now what releases are built with, put these hashes and versions in tools\build-ffmpeg.ps1 and run $command again."
    } else {
        "They were built with $pinned, the versions the pins are from, so something else in the build has changed; the log is $log."
    }
    throw "the DLLs differ from the pins in this script: $($differs -join '; '). $why Nothing in $target was changed; the build is in $made."
}

# --- third_party\ffmpeg ---------------------------------------------------

$staged = "$target.new"
$old = "$target.old"
foreach ($folder in $staged, $old) {
    if (Test-Path -LiteralPath $folder) { Remove-Item -LiteralPath $folder -Recurse -Force }
}
New-Item -ItemType Directory -Force (Join-Path $staged 'bin'), (Join-Path $staged 'include') | Out-Null
foreach ($dll in $dllPins.Keys) { Copy-Item -LiteralPath (Join-Path $made "bin\$dll") -Destination (Join-Path $staged 'bin') }
foreach ($headers in 'libavcodec', 'libavutil') {
    Copy-Item -LiteralPath (Join-Path $made "include\$headers") -Destination (Join-Path $staged 'include') -Recurse
}
# The license the DLLs say they are under, LGPL version 2.1 or later, in the
# text the source archive carries.
Copy-Item -LiteralPath (Join-Path $sourceDir 'COPYING.LGPLv2.1') -Destination (Join-Path $staged 'LICENSE.txt')
# The notices of the few files in them under other licenses, which build.sh
# collects.
Copy-Item -LiteralPath (Join-Path $made 'NOTICES.txt') -Destination $staged
# The exact source goes with the DLLs, and tools\release.ps1 -FfmpegSource
# takes it from here into FFmpeg's source release.
Copy-Item -LiteralPath $archive -Destination (Join-Path $staged $archiveName)

$packageNames = @($packages.Keys | ForEach-Object { $_ -replace '-x86_64\.pkg\.tar\.zst$', '' })
$packageList = ($packageNames[0..($packageNames.Count - 2)] -join ', ') + ' and ' + $packageNames[-1]
$lines = @(
    "FFmpeg $version, built for Booth from FFmpeg's unmodified source by tools\build-ffmpeg.ps1: avcodec and avutil only, with the H.264 and HEVC decoders and their Direct3D 11 hardware paths.",
    "Build: $found from $product; $(($msys2Url -replace '^.*/', '') -replace '\.tar\.xz$', '') with $packageList from MSYS2 for the shell, make and nasm"
)
foreach ($dll in $hashes.Keys) { $lines += "SHA-256 ${dll}: $($hashes[$dll])" }
$lines += @(
    "Source: $sourceUrl",
    "Source SHA-256: $sourceSha256",
    $recipeLine,
    "build.sh SHA-256: $(Get-Sha256 $recipe)"
)
[System.IO.File]::WriteAllText((Join-Path $staged 'SOURCE.txt'), (($lines -join "`r`n") + "`r`n"), (New-Object System.Text.UTF8Encoding $false))

if (Test-Path -LiteralPath $target) { Move-Folder $target $old }
try {
    Move-Folder $staged $target
} catch {
    if (Test-Path -LiteralPath $old) { Move-Folder $old $target }
    throw
}
if (Test-Path -LiteralPath $old) {
    try {
        Remove-Item -LiteralPath $old -Recurse -Force
    } catch {
        Write-Warning "the new build is in place, but the old one could not be removed: $($_.Exception.Message) Delete $old by hand."
    }
}
# About 200 MB, kept only when a build fails; the log stays either way.
Remove-Item -LiteralPath $build -Recurse -Force -ErrorAction SilentlyContinue

$minutes = ((Get-Date) - $started).TotalMinutes
Write-Host ("FFmpeg {0} is in {1}, built in {2:N1} minutes:" -f $version, $target, $minutes)
foreach ($dll in $hashes.Keys) {
    Write-Host ('  {0,-16} {1,12:N0} bytes  {2}' -f $dll, (Get-Item -LiteralPath (Join-Path $target "bin\$dll")).Length, $hashes[$dll])
}
