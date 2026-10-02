# Builds a release with one command:
#
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -SecretKey E:\booth-release.key
#
# Windows runs no scripts until told otherwise, and -ExecutionPolicy Bypass
# tells it for that one command only.
#
# Checks every crate with cargo deny, builds the release tool and checks
# with it that the key's public half is RELEASE_KEY, the key every copy of
# Booth checks a release against. Only then fetches the third-party files,
# collects their licenses, builds booth.exe, puts the zip together, builds
# the installer when Inno Setup 6 is installed, writes the update manifest
# latest.txt with the SHA-256 of both, and signs latest.txt and nothing
# else, since it vouches for the other two. The release is those four files
# in dist\<version>: the zip, the installer, latest.txt and
# latest.txt.minisig. The key's password is asked for twice, for the check
# and for signing, three tries each. It uploads and publishes nothing; that
# stays a step done by hand.
#
# dist\<version> is emptied before anything is built, so it never holds an
# earlier run's files beside this one's, such as an installer latest.txt
# does not describe. Emptied rather than refused: only this script writes
# there and it makes every file again, so a refusal would only mean a
# delete by hand before each rebuild.
#
# No folder of this PC goes out with a release. The cargo home, the
# repository and the target folder are cut from the file names rustc and
# cl.exe write into booth.exe, and every file for the zip and the installer
# is searched for them, and for any :\Users, before it is zipped. A hit
# stops the release.
#
# FFmpeg's source archive and the recipe that built its DLLs go up once, to
# a release of their own that the license texts in every zip point to, not
# with each release. -FfmpegSource puts those two files in
# dist\ffmpeg-<FFmpeg's version> and does nothing else: no key, no build,
# no signature.
#
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -FfmpegSource
#
# The key is made once with the release tool, and the public key it prints
# goes in RELEASE_KEY in crates\app\src\update\mod.rs:
#   cargo run --release --locked -p release -- keygen E:\booth-release.key
#
# -Rehearsal builds a release that is never to be published. Where a real
# run stops, it warns and goes on: while the releases address in the code
# still has its OWNER placeholder or FFmpeg's source release is not under
# it, while RELEASE_KEY is not made yet, and with a key that is not
# RELEASE_KEY's, so a throwaway key works.
param(
    [string]$SecretKey,
    [switch]$Rehearsal,
    [switch]$FfmpegSource
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$cargoDenyVersion = '0.20.2'

# Output from cargo and the release tool goes straight to the console, not
# through PowerShell, which in Windows PowerShell turns a native program's
# progress lines on stderr into errors when the output is redirected. The
# working folder is the repository's, where cargo finds .cargo\config.toml
# and rust-toolchain.toml.
function Invoke-Tool([string]$What, [string]$Exe, [string[]]$Arguments, [hashtable]$Environment = @{}) {
    $quoted = foreach ($argument in $Arguments) {
        if ($argument -eq '' -or $argument -match '[\s"]') { '"' + ($argument -replace '"', '\"') + '"' } else { $argument }
    }
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $Exe
    $info.Arguments = $quoted -join ' '
    $info.WorkingDirectory = $root
    $info.UseShellExecute = $false
    foreach ($name in $Environment.Keys) { $info.EnvironmentVariables[$name] = $Environment[$name] }
    $process = [System.Diagnostics.Process]::Start($info)
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        throw "$What failed with exit code $($process.ExitCode); nothing after it was done."
    }
}

# For the two short questions asked of git and cargo-deny, whose stderr is
# not wanted and must not stop the script.
function Get-ToolOutput([string]$Exe, [string[]]$Arguments) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Exe @Arguments 2>$null } finally { $ErrorActionPreference = $saved }
}

function Write-Crlf([string]$Path, [string[]]$Lines) {
    $text = ($Lines -join "`r`n") + "`r`n"
    [System.IO.File]::WriteAllText($Path, $text, (New-Object System.Text.UTF8Encoding $false))
}

function Format-Size([long]$Bytes) {
    if ($Bytes -lt 1MB) { '{0:N0} KB' -f [math]::Ceiling($Bytes / 1KB) } else { '{0:N1} MB' -f ($Bytes / 1MB) }
}

function Write-Listing([string]$Folder) {
    foreach ($item in Get-ChildItem -LiteralPath $Folder -File | Sort-Object Name) {
        Write-Host ('  {0,-44} {1,10}' -f $item.Name, (Format-Size $item.Length))
    }
}

function Clear-Folder([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        $old = @(Get-ChildItem -LiteralPath $Path -Recurse -File -Force)
        Write-Host "emptying $Path, which held $($old.Count) files from an earlier run"
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
    New-Item -ItemType Directory -Force $Path | Out-Null
}

# System.IO.Compression and not Compress-Archive: the Compress-Archive in
# Windows PowerShell writes backslashes into entry names, which other unzip
# tools turn into file names with backslashes in them.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
function New-Zip([string]$Path, [string]$Base, [System.IO.FileInfo[]]$Items) {
    $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::CreateNew)
    $archive = New-Object System.IO.Compression.ZipArchive($stream, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($item in $Items) {
            $entry = $item.FullName.Substring($Base.Length + 1).Replace('\', '/')
            [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive, $item.FullName, $entry, [System.IO.Compression.CompressionLevel]::Optimal)
        }
    } finally {
        $archive.Dispose()
        $stream.Dispose()
    }
}

$rebuildFfmpeg = 'Build FFmpeg again with: powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1'

# The LGPL asks for the exact source of the FFmpeg DLLs for as long as they
# are offered. third_party\ffmpeg\SOURCE.txt, which build-ffmpeg.ps1 writes
# with the DLLs, names the archive they were built from and its SHA-256, the
# recipe zip, build.sh's SHA-256 and the release that holds the archive and
# the recipe, which the zip of every release points to. build-ffmpeg.ps1
# has that release's address written into it, so its recipe runs on its
# own, and it is held here to the app's RELEASES_PAGE, so the two cannot
# drift apart.
function Read-FfmpegSource {
    $path = Join-Path $root 'third_party\ffmpeg\SOURCE.txt'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "$path does not exist, so there is no FFmpeg build to give the source of. $rebuildFfmpeg"
    }
    $plainName = '[A-Za-z0-9][A-Za-z0-9._-]*'
    $source = [ordered]@{ Archive = $null; ArchiveSha256 = $null; Recipe = $null; Release = $null; Address = $null; BuildShSha256 = $null }
    foreach ($line in Get-Content -LiteralPath $path) {
        if ($line -match "^Source: https://\S+/($plainName)$") { $source.Archive = $Matches[1] }
        elseif ($line -match '^Source SHA-256: ([0-9A-Fa-f]{64})$') { $source.ArchiveSha256 = $Matches[1] }
        elseif ($line -match "^Recipe: .* as ($plainName\.zip), in the release ($plainName) at (https://\S+) ") {
            $source.Recipe = $Matches[1]
            $source.Release = $Matches[2]
            $source.Address = $Matches[3]
        }
        elseif ($line -match '^build\.sh SHA-256: ([0-9A-Fa-f]{64})$') { $source.BuildShSha256 = $Matches[1] }
    }
    if (@($source.Values | Where-Object { -not $_ }).Count -gt 0) {
        throw "$path does not name the source archive, its SHA-256, the recipe zip, the release they go in and build.sh's SHA-256. $rebuildFfmpeg"
    }
    $expected = "$releasesPage/tag/$($source.Release)"
    if ($source.Address -cne $expected) {
        $problem = "$path says FFmpeg's source and recipe are in the release at $($source.Address), not at $expected under RELEASES_PAGE in $versionCode, so the zip would send people to a release this repository does not have. Make `$releasesPage in tools\build-ffmpeg.ps1 the same as RELEASES_PAGE, then: powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1"
        if (-not $Rehearsal) { throw $problem }
        Write-Warning "$problem This rehearsal goes on anyway; do not publish it."
    }
    [pscustomobject]$source
}

if ($FfmpegSource -and ($SecretKey -or $Rehearsal)) {
    throw '-FfmpegSource builds and signs nothing, so it takes no -SecretKey and no -Rehearsal. Run it on its own: powershell -ExecutionPolicy Bypass -File tools\release.ps1 -FfmpegSource'
}

$git = Get-Command git -ErrorAction SilentlyContinue
if ($git) {
    $changes = Get-ToolOutput $git.Source @('-C', $root, 'status', '--porcelain')
    if ($changes) {
        Write-Warning 'the working tree has changes that are not committed, so what this run makes does not come from a commit.'
    }
}

# The address latest.txt points the zip at, and the one FFmpeg's source
# release has to be under, is the one the app sends friends to and checks
# for updates at, so it is read from there.
$versionCode = Join-Path $root 'crates\invite\src\version.rs'
$releasesPage = $null
foreach ($line in Get-Content -LiteralPath $versionCode) {
    if ($line -match '^\s*pub const RELEASES_PAGE: &str = "(https://[^"\s]+)";') { $releasesPage = $Matches[1].TrimEnd('/'); break }
}
if (-not $releasesPage) {
    throw "could not read RELEASES_PAGE from $versionCode; latest.txt takes the address of the zip from there, and FFmpeg's source release has to be under it"
}

# --- FFmpeg's source release, with -FfmpegSource --------------------------

# The archive and the recipe zip, which is tools\build-ffmpeg.ps1 and
# tools\ffmpeg.
if ($FfmpegSource) {
    $source = Read-FfmpegSource
    $archive = Join-Path $root "third_party\ffmpeg\$($source.Archive)"
    if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "$archive is missing. $rebuildFfmpeg"
    }
    $got = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    if ($got -ne $source.ArchiveSha256) {
        throw "$archive has SHA-256 $got, not the $($source.ArchiveSha256) SOURCE.txt gives, so it is not the source the DLLs were built from. $rebuildFfmpeg"
    }
    # The recipe has to be the one that built the DLLs. SOURCE.txt pins
    # build.sh, which holds every option; build-ffmpeg.ps1 only runs it.
    $buildSh = Join-Path $PSScriptRoot 'ffmpeg\build.sh'
    if ((Get-FileHash -LiteralPath $buildSh -Algorithm SHA256).Hash -ne $source.BuildShSha256) {
        throw "tools\ffmpeg\build.sh has changed since third_party\ffmpeg was built, so the recipe zip would not be the recipe that built the DLLs. $rebuildFfmpeg"
    }

    $out = Join-Path $root "dist\$($source.Release)"
    Clear-Folder $out
    Copy-Item -LiteralPath $archive -Destination $out
    $recipeFiles = @(Get-Item -LiteralPath (Join-Path $PSScriptRoot 'build-ffmpeg.ps1')) +
        @(Get-ChildItem -LiteralPath (Join-Path $PSScriptRoot 'ffmpeg') -Recurse -File | Sort-Object FullName)
    New-Zip (Join-Path $out $source.Recipe) $root $recipeFiles

    $title = 'FFmpeg {0} source' -f ($source.Release -replace '^ffmpeg-', '')
    Write-Host ''
    Write-Host "FFmpeg's source release is in $out, not published:"
    Write-Listing $out
    Write-Host "$($source.Archive) SHA-256: $($source.ArchiveSha256.ToLowerInvariant()), as SOURCE.txt gives it"
    Write-Host "Upload both once, before the first Booth release that points to them, to a new release with the tag $($source.Release) and the title `"$title`", with `"Set as the latest release`" not ticked: the update check reads latest.txt from the release marked latest. Every Booth zip sends people to $($source.Address) for them."
    Write-Host "That release keeps this recipe for good. A changed tools\ffmpeg\build.sh needs a source release under a new name, set in tools\build-ffmpeg.ps1, not new files in this one."
    return
}

# --- What the release needs before anything is built ---------------------

if (-not $SecretKey) {
    throw 'give the release key: powershell -ExecutionPolicy Bypass -File tools\release.ps1 -SecretKey <path to the secret key>. Make one once with: cargo run --release --locked -p release -- keygen <path outside this repository>'
}
if (-not (Test-Path -LiteralPath $SecretKey -PathType Leaf)) {
    throw "the release key $SecretKey does not exist. Plug in the drive it is on, or make one with the release tool's keygen."
}
$SecretKey = (Resolve-Path -LiteralPath $SecretKey).Path

$manifest = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml')
$inPackage = $false
$version = $null
foreach ($line in $manifest) {
    if ($line -match '^\s*\[') { $inPackage = ($line -match '^\s*\[workspace\.package\]\s*$') }
    elseif ($inPackage -and $line -match '^\s*version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') { $version = $Matches[1]; break }
}
if (-not $version) {
    throw 'could not read the version from [workspace.package] in Cargo.toml; it must be three numbers, like 0.1.0'
}

# The public key every copy of Booth checks a release against, read from
# the app the same way. The release tool compares the key with it.
$updateCode = Join-Path $root 'crates\app\src\update\mod.rs'
$releaseKey = $null
foreach ($line in Get-Content -LiteralPath $updateCode) {
    if ($line -match '^\s*pub const RELEASE_KEY: &str = "([^"]*)";') { $releaseKey = $Matches[1]; break }
}
if ($null -eq $releaseKey) {
    throw "could not read RELEASE_KEY from $updateCode; the release key is checked against it"
}

# Both are told at once, so a first real run does not stop twice.
$notReady = @()
if ($releasesPage -match '/OWNER/') {
    $notReady += "RELEASES_PAGE in $versionCode is still $releasesPage, so latest.txt would point nowhere. Put the public repository's owner in it."
}
if ($releaseKey -cnotmatch '^RW[A-Za-z0-9+/]{54}$') {
    $notReady += "RELEASE_KEY in $updateCode is `"$releaseKey`", not a public key yet, so no copy of Booth could check this release. Make the release key once with: cargo run --release --locked -p release -- keygen <path outside this repository>, and put the public key it prints in RELEASE_KEY."
}
if ($notReady) {
    if (-not $Rehearsal) {
        throw (($notReady + 'Or pass -Rehearsal for a build that is never published.') -join "`n")
    }
    foreach ($problem in $notReady) {
        Write-Warning "$problem This rehearsal goes on anyway; do not publish it."
    }
}

foreach ($file in 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE') {
    if (-not (Test-Path -LiteralPath (Join-Path $root $file) -PathType Leaf)) {
        throw "$file is missing at the repository root, and the zip needs it."
    }
}

$denyVersion = $null
$denyCommand = Get-Command cargo-deny -ErrorAction SilentlyContinue
if ($denyCommand) {
    $denyVersion = (Get-ToolOutput $denyCommand.Source @('--version')) -replace '^cargo-deny\s+', ''
}
if ($denyVersion -ne $cargoDenyVersion) {
    $found = if ($denyVersion) { "cargo-deny $denyVersion is installed" } else { 'cargo-deny is not installed' }
    throw "$found; the release checks with $cargoDenyVersion. Install it with: cargo install --locked cargo-deny@$cargoDenyVersion"
}

# Inno Setup is optional: without it there is no installer. Its own
# installer offers a folder of the user's choosing, and its uninstall entry,
# per user or for everyone, says which one was picked.
$innoFolders = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6'),
    (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6'),
    (Join-Path $env:ProgramFiles 'Inno Setup 6')
)
foreach ($hive in 'HKCU:\Software', 'HKLM:\Software\WOW6432Node', 'HKLM:\Software') {
    $entry = Get-ItemProperty -LiteralPath "$hive\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1" -ErrorAction SilentlyContinue
    if ($entry -and $entry.InstallLocation) { $innoFolders += $entry.InstallLocation }
}
$iscc = $innoFolders | ForEach-Object { Join-Path $_ 'ISCC.exe' } |
    Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1

# ISCC checks its compiler DLLs and the Setup code it puts in the installer
# against Inno Setup's own signatures, so a genuine ISCC.exe vouches for the
# rest. booth.iss checks the version.
if ($iscc) {
    $signature = Get-AuthenticodeSignature -LiteralPath $iscc
    $signer = if ($signature.SignerCertificate) { $signature.SignerCertificate.GetNameInfo('SimpleName', $false) } else { 'nobody' }
    if ($signature.Status -ne 'Valid' -or $signer -cne 'Pyrsys B.V.') {
        throw "$iscc is not signed by Pyrsys B.V. (signature $($signature.Status), signer $signer), and the installer it builds would be named in the signed latest.txt. Reinstall Inno Setup 6.7.3 from jrsoftware.org for this user only."
    }
}

# --- This PC's folders, which no file of the release may hold -------------

# Cargo resolves a relative CARGO_HOME or CARGO_TARGET_DIR against the
# folder it runs in, which is the repository's.
function Resolve-FromRoot([string]$Path) {
    if ([System.IO.Path]::IsPathRooted($Path)) { $Path } else { Join-Path $root $Path }
}
$target = if ($env:CARGO_TARGET_DIR) { Resolve-FromRoot $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
$cargoHome = if ($env:CARGO_HOME) { Resolve-FromRoot $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$buildFolders = @($cargoHome, $root, $target | ForEach-Object { $_.TrimEnd('\', '/') } | Select-Object -Unique)

# rustc writes the full path of a downloaded crate's source into booth.exe
# for every place it can panic from, and of any file include!d from a build
# folder; libopus writes __FILE__ into it. Cargo's trim-paths would cut
# them but is not stable yet, so rustc gets a --remap-path-prefix and
# cl.exe a /d1trimfile for each folder, which cut it from the front of
# every path. rustc applies the last prefix that matches and cl.exe the
# first, so the longest goes last for one and first for the other. The
# rustflags go in with --config, which adds them to the +crt-static in
# .cargo\config.toml; RUSTFLAGS would replace that.
foreach ($name in 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS') {
    if ([Environment]::GetEnvironmentVariable($name)) {
        throw "$name is set, and it replaces the rustflags in .cargo\config.toml and the ones this script adds, so booth.exe would need the Visual C++ runtime and hold this PC's folders. Clear it for the release: Remove-Item Env:$name"
    }
}
# cc and the cmake crate split C flags at spaces.
foreach ($folder in $buildFolders) {
    if ($folder -match '\s') {
        throw "$folder has a space in it, which no C flag can carry, so libopus would write it into booth.exe. Build from folders without spaces; CARGO_HOME and CARGO_TARGET_DIR move the cargo home and the target folder."
    }
}
$remaps = foreach ($folder in $buildFolders | Sort-Object Length) {
    '"--remap-path-prefix=' + $folder.Replace('\', '\\') + '="'
}
$buildArgs = @('--config', ('target.x86_64-pc-windows-msvc.rustflags=[' + ($remaps -join ', ') + ']'))
$trims = foreach ($folder in $buildFolders | Sort-Object Length -Descending) { "/d1trimfile:$folder\" }
$buildEnvironment = @{ CFLAGS = ((@($env:CFLAGS) + $trims) -join ' ').Trim() }

# The search. Text with one byte per letter is read as Latin-1, byte for
# byte, and a folder is looked for as rustc writes it, in UTF-8, and as
# cl.exe writes __FILE__, in the ANSI code page; for an ASCII folder both
# are the folder itself. UTF-16 is read from both an even and an odd byte,
# since a string need not start on an even one. Either slash and any case
# count. The repository folder's name alone counts only next to a
# backslash: a clone named booth also has /booth/ in the releases address.
$latin1 = [System.Text.Encoding]::GetEncoding(28591)
$searchFolders = @(
    @{ What = 'the user profile'; Path = $env:USERPROFILE },
    @{ What = 'the cargo home'; Path = $cargoHome },
    @{ What = 'the repository'; Path = $root },
    @{ What = 'the target folder'; Path = $target }
) | Where-Object { $_.Path }
$repositoryName = Split-Path -Leaf $root.TrimEnd('\', '/')

# Searched for on its own, the folder's name has to be one the build does
# not write anyway. rustc writes the folders of the standard library, the
# registry, the target folder and this repository's own crates into
# booth.exe, as in library\core\src, registry\src, release\build and
# crates\room\src, and a name of one or two letters turns up by chance in
# any binary file.
$writtenAnyway = @('library', 'core','alloc', 'std', 'src', 'registry', 'release', 'build', 'out', 'crates', 'vendor') +
    @('crates', 'vendor' | ForEach-Object { Join-Path $root $_ } | Where-Object { Test-Path -LiteralPath $_ } |
        ForEach-Object { Get-ChildItem -LiteralPath $_ -Recurse -Directory } | ForEach-Object { $_.Name })
if ($repositoryName.Length -lt 3 -or $writtenAnyway -contains $repositoryName) {
    throw "the repository is in a folder named $repositoryName, a name the build writes into booth.exe as a folder of its own or one short enough to turn up there by chance, so the search for this PC's folders would stop the release over it. Move the clone to a folder with another name, such as booth."
}

function Get-Forms([string]$Text, [bool]$OneByte) {
    if (-not $OneByte) { return , $Text }
    [System.Text.Encoding]::UTF8, [System.Text.Encoding]::Default |
        ForEach-Object { $latin1.GetString($_.GetBytes($Text)) } | Select-Object -Unique
}

function New-Needle([string]$What, [string[]]$Patterns, [string]$Advice) {
    $options = [System.Text.RegularExpressions.RegexOptions]'IgnoreCase, CultureInvariant, Compiled'
    [pscustomobject]@{ What = $What; Regex = New-Object regex ('(?:' + ($Patterns -join '|') + ')'), $options; Advice = $Advice }
}

function Get-Needles([bool]$OneByte) {
    New-Needle ':\Users' @(':[\\/]+Users\b')
    foreach ($folder in $searchFolders) {
        $patterns = foreach ($form in Get-Forms $folder.Path $OneByte) {
            (@($form -split '[\\/]+' | Where-Object { $_ }) | ForEach-Object { [regex]::Escape($_) }) -join '[\\/]+'
        }
        New-Needle "$($folder.What) $($folder.Path)" $patterns
    }
    if ($repositoryName) {
        $patterns = foreach ($form in Get-Forms $repositoryName $OneByte) {
            $name = [regex]::Escape($form)
            '\\' + $name + '[\\/]|[\\/]' + $name + '\\'
        }
        # The check above knows only the folders of the standard library and
        # of this repository, not those inside every crate.
        New-Needle "the repository folder's name $repositoryName between slashes" $patterns "If a hit on the name $repositoryName sits in a crate's own path, such as registry\src\..., it is that crate's own folder of the same name, not one of this PC's: move the clone to a folder with another name."
    }
}
$oneByteNeedles = @(Get-Needles $true)
$wideNeedles = @(Get-Needles $false)

# The printable run a match sits in, so a hit shows the whole path.
function Get-Around([string]$Text, [System.Text.RegularExpressions.Match]$Match) {
    $start = $Match.Index
    while ($start -gt 0 -and $Match.Index - $start -lt 80 -and $Text[$start - 1] -match '[ -~]') { $start-- }
    $end = $Match.Index + $Match.Length
    while ($end -lt $Text.Length -and $end - $Match.Index -lt 200 -and $Text[$end] -match '[ -~]') { $end++ }
    $Text.Substring($start, $end - $start)
}

function Assert-NoBuildFolder([System.IO.FileInfo[]]$Files) {
    $hits = @()
    $advice = @()
    foreach ($file in $Files) {
        $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
        $texts = @(@{ Kind = 'one-byte text'; Text = $latin1.GetString($bytes); Needles = $oneByteNeedles })
        if ($bytes.Length -ge 2) {
            $texts += @{ Kind = 'UTF-16 text'; Text = [System.Text.Encoding]::Unicode.GetString($bytes); Needles = $wideNeedles }
            $texts += @{ Kind = 'UTF-16 text from an odd byte'; Text = [System.Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1); Needles = $wideNeedles }
        }
        foreach ($text in $texts) {
            foreach ($needle in $text.Needles) {
                $found = $needle.Regex.Matches($text.Text)
                if ($found.Count -gt 0) {
                    $hits += "$($file.FullName) holds $($needle.What): $($found.Count) in $($text.Kind), the first in $(Get-Around $text.Text $found[0])"
                    if ($needle.Advice) { $advice += $needle.Advice }
                }
            }
        }
    }
    if ($hits) {
        throw ((@("This release would carry folders of this PC, so it stops here. Do not upload anything from $dist.") + $hits + @($advice | Select-Object -Unique)) -join "`n")
    }
    Write-Host "none of this PC's folders in: $(($Files | ForEach-Object { $_.Name }) -join ', ')"
}

Write-Host "Booth $version"

$dist = Join-Path $root "dist\$version"
$staging = Join-Path $root 'dist\staging'
Clear-Folder $dist
if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }

# --- The crate check and the release key ---------------------------------

Invoke-Tool 'cargo deny' 'cargo' @('deny', '--locked', 'check', '--hide-inclusion-graph')

$booth = Join-Path $target 'release\booth.exe'
$releaseTool = Join-Path $target 'release\release.exe'
$rehearsalArgs = @()
if ($Rehearsal) { $rehearsalArgs = @('--rehearsal') }

# A password-protected key shows its public half only once it is opened,
# which takes the release tool, so that small build comes first. Two cargo
# builds, here and for booth.exe below, because one build of both would
# settle shared crates' features for both, and booth.exe would not be the
# -p app build the license list describes. Both get the path flags, or the
# crates they share would be built again for each.
Invoke-Tool 'the release tool build' 'cargo' (@('build', '--release', '--locked', '-p', 'release') + $buildArgs) $buildEnvironment
Invoke-Tool 'the release key check' $releaseTool (@('check-key') + $rehearsalArgs + @($SecretKey, $releaseKey))

# --- Third-party files, the build and the zip -----------------------------

& (Join-Path $PSScriptRoot 'fetch-third-party.ps1')
$ffmpegRelease = Read-FfmpegSource

$name = "booth-$version-windows-x64"
$files = Join-Path $staging $name
New-Item -ItemType Directory -Force $files | Out-Null

# The licenses first: they refuse an FFmpeg build that cannot ship, which
# is quicker to hear before the long build than after it.
Invoke-Tool 'collecting the third-party licenses' $releaseTool @('licenses', $root, $files)
Invoke-Tool 'the release build' 'cargo' (@('build', '--release', '--locked', '-p', 'app') + $buildArgs) $buildEnvironment

Copy-Item -LiteralPath $booth -Destination $files
Copy-Item -Path (Join-Path $root 'third_party\ffmpeg\bin\*.dll') -Destination $files

# The zip's ffmpeg\SOURCE.txt gives each FFmpeg DLL's SHA-256 as the build
# wrote it, and the release tool checks only that the lines are there, so
# the DLLs going into the zip are held to them here.
$ffmpegHashes = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $files 'ffmpeg\SOURCE.txt')) {
    if ($line -match '^SHA-256 (\S+\.dll): ([0-9A-Fa-f]{64})$') { $ffmpegHashes[$Matches[1]] = $Matches[2] }
}
foreach ($dll in Get-ChildItem -LiteralPath $files -Filter '*.dll' -File) {
    $got = (Get-FileHash -LiteralPath $dll.FullName -Algorithm SHA256).Hash
    if (-not $ffmpegHashes.ContainsKey($dll.Name) -or $got -ne $ffmpegHashes[$dll.Name]) {
        throw "$($dll.Name) from third_party\ffmpeg\bin has SHA-256 $got, which is not the one third_party\ffmpeg\SOURCE.txt gives it, so the zip would describe other DLLs than it holds. Build FFmpeg again with: powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1"
    }
}

foreach ($file in 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE') {
    Copy-Item -LiteralPath (Join-Path $root $file) -Destination $files
}

# The installer is built from this same folder, so this covers both.
$releaseFiles = @(Get-ChildItem -LiteralPath $files -Recurse -File | Sort-Object FullName)
Assert-NoBuildFolder $releaseFiles

$zip = Join-Path $dist "$name.zip"
New-Zip $zip $files $releaseFiles
$sha256 = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()

# --- The installer, when Inno Setup is there ------------------------------

# Before latest.txt, which gives its SHA-256. Without Inno Setup the release
# is the other three files, and latest.txt has no installer lines, which the
# update check takes as a release without an installer.
$installerName = "booth-$version-setup.exe"
$installerSha256 = $null
if ($iscc) {
    Invoke-Tool 'the installer build' $iscc @(
        '/Q', "/DAppVersion=$version", "/DSourceDir=$files", "/DOutputDir=$dist", (Join-Path $PSScriptRoot 'booth.iss'))
    $installer = Join-Path $dist $installerName
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw "the installer build ended without $installer; booth.iss has to name its output booth-<version>-setup.exe, which latest.txt gives the SHA-256 of."
    }
    $installerSha256 = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
} else {
    Write-Host 'Inno Setup 6 was not found in the programs folders or in its uninstall entry, so there is no installer this time: latest.txt names only the zip, and the release is three files. Install Inno Setup 6.7.3 from jrsoftware.org for this user only to build one.'
}

# And the zip and the installer themselves, whose entry names and Setup
# program are not compressed.
Assert-NoBuildFolder @(Get-ChildItem -LiteralPath $dist -File)

# --- latest.txt and its signature -----------------------------------------

# The update check reads this strictly (crates\app\src\update\manifest.rs):
# these five keys once each, the installer's two together or not at all, and
# nothing else. It downloads only the zip; the installer's lines are there
# for checking a download by hand, as the release notes say.
$latest = Join-Path $dist 'latest.txt'
$latestLines = @(
    "version = $version",
    "published = $((Get-Date).ToUniversalTime().ToString('yyyy-MM-dd'))",
    "zip = $name.zip",
    "sha256 = $sha256",
    "url = $releasesPage/download/v$version/$name.zip"
)
if ($installerSha256) {
    $latestLines += "installer = $installerName", "installer_sha256 = $installerSha256"
}
Write-Crlf $latest $latestLines

Invoke-Tool 'signing latest.txt' $releaseTool (@('sign') + $rehearsalArgs + @($SecretKey, $releaseKey, $latest))

Remove-Item -LiteralPath $staging -Recurse -Force

$expected = @("$name.zip", 'latest.txt', 'latest.txt.minisig')
if ($installerSha256) { $expected += $installerName }
$present = @(Get-ChildItem -LiteralPath $dist -Force | ForEach-Object { $_.Name })
$strays = @($present | Where-Object { $expected -notcontains $_ })
$missing = @($expected | Where-Object { $present -notcontains $_ })
if ($strays -or $missing) {
    throw "$dist should hold $($expected -join ', ') and nothing else, but it is missing [$($missing -join ', ')] and also holds [$($strays -join ', ')]. Do not upload it; run the release again."
}

Write-Host ''
Write-Host "Release $version is in $dist, not published:"
Write-Listing $dist
Write-Host "zip SHA-256: $sha256"
if ($installerSha256) { Write-Host "installer SHA-256: $installerSha256" }
Write-Host "FFmpeg's source archive and recipe are not part of it. The zip sends people to $($ffmpegRelease.Address) for them, so while that release is not up yet, gather it first with: powershell -ExecutionPolicy Bypass -File tools\release.ps1 -FfmpegSource, and upload it before this one. Then upload these files to the release with the tag v$version, marked as the latest release."
