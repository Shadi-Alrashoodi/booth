# Builds the Microsoft Store package:
#
#   powershell -ExecutionPolicy Bypass -File tools\store.ps1
#
# The result is dist\store\booth-<version>.msix, unsigned: the Store signs
# what it is given. It holds booth.exe built with the store feature (no
# update check, no firewall step), the two FFmpeg DLLs, the license files
# the zip has, the logos, and tools\store\AppxManifest.xml filled in with the
# version and the identity in tools\store\identity.txt. Fill that file in
# from Partner Center first; without it there is nothing the Store would
# take.
#
# -Test builds dist\store\booth-<version>-test.msix under an identity of its
# own, Name BoothLocalTest and Publisher CN=Booth local test, for trying the
# package on this PC. It installs only once signed with a certificate of
# that subject which Windows trusts, and is never for the Store.
#
# The zip, the installer and dist\<version> are release.ps1's and are not
# touched. booth.exe is built in target\store, so target\release keeps the
# normal build.
#
# The logos are drawn here from docs\mark.svg, each size on its own pixel
# grid, as booth.ico is: an edge at grid line c of 16 lands on pixel
# c * size / 16, rounded half down, and the right and bottom edges mirror
# the left and top ones so the mark stays centred. Icons get the same panel
# plate booth.ico has, tiles a full panel background with the mark in the
# middle half.
#
# Needs the Windows 10 or 11 SDK for makeappx and makepri.
param(
    [switch]$Test
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

# -Quiet keeps a tool's output, which for makeappx is a line per file, and
# shows it only when the tool fails.
function Invoke-Tool([string]$What, [string]$Exe, [string[]]$Arguments, [hashtable]$Environment = @{}, [switch]$Quiet) {
    $quoted = foreach ($argument in $Arguments) {
        if ($argument -eq '' -or $argument -match '[\s"]') { '"' + ($argument -replace '"', '\"') + '"' } else { $argument }
    }
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $Exe
    $info.Arguments = $quoted -join ' '
    $info.WorkingDirectory = $root
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = [bool]$Quiet
    foreach ($name in $Environment.Keys) { $info.EnvironmentVariables[$name] = $Environment[$name] }
    $process = [System.Diagnostics.Process]::Start($info)
    $output = if ($Quiet) { $process.StandardOutput.ReadToEnd() }
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        if ($output) { Write-Host $output }
        throw "$What failed with exit code $($process.ExitCode); nothing after it was done."
    }
}

function Write-Utf8([string]$Path, [string]$Text) {
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding $false))
}

# --- Version and identity -------------------------------------------------

$version = $null
$inPackage = $false
foreach ($line in Get-Content -LiteralPath (Join-Path $root 'Cargo.toml')) {
    if ($line -match '^\s*\[') { $inPackage = ($line -match '^\s*\[workspace\.package\]\s*$') }
    elseif ($inPackage -and $line -match '^\s*version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') { $version = $Matches[1]; break }
}
if (-not $version) {
    throw 'could not read the version from [workspace.package] in Cargo.toml; it must be three numbers, like 1.0.0'
}
# The Store wants four numbers and the last one 0; it keeps that one for itself.
$packageVersion = "$version.0"

$identityFile = Join-Path $PSScriptRoot 'store\identity.txt'
$identity = [ordered]@{ Name = ''; Publisher = ''; PublisherDisplayName = '' }
foreach ($line in Get-Content -LiteralPath $identityFile) {
    if ($line -match '^\s*(#|$)') { continue }
    if ($line -notmatch '^\s*(\w+)\s*=\s*(.*?)\s*$' -or -not $identity.Contains($Matches[1])) {
        throw "$identityFile has a line that is not Name, Publisher or PublisherDisplayName = value: $line"
    }
    $identity[$Matches[1]] = $Matches[2]
}

if ($Test) {
    $identity.Name = 'BoothLocalTest'
    $identity.Publisher = 'CN=Booth local test'
    if (-not $identity.PublisherDisplayName) { $identity.PublisherDisplayName = 'Shadi Alrashoodi' }
    $packageName = "booth-$version-test.msix"
} else {
    $empty = @($identity.Keys | Where-Object { -not $identity[$_] })
    if ($empty) {
        throw "$identityFile has no value for $($empty -join ', '). Copy them from Partner Center, Product identity, View app identity details. Or pass -Test for a package to try on this PC only."
    }
    $packageName = "booth-$version.msix"
}
# The rules makeappx holds the manifest to, said here in words instead.
if ($identity.Name -notmatch '^[A-Za-z0-9.-]{3,50}$') {
    throw "the Name in $identityFile is `"$($identity.Name)`"; a package name is 3 to 50 letters, digits, dots and hyphens, as Partner Center shows it."
}
if ($identity.Publisher -notmatch '^CN=') {
    throw "the Publisher in $identityFile is `"$($identity.Publisher)`"; it starts with CN=, as Partner Center shows it."
}

# --- The SDK's tools ------------------------------------------------------

$kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
$sdkBin = Get-ChildItem -LiteralPath $kits -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^10\.0\.\d+\.\d+$' -and (Test-Path (Join-Path $_.FullName 'x64\makeappx.exe')) } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $sdkBin) {
    throw "makeappx.exe is not under $kits. Install the Windows 11 SDK (the Visual Studio Installer lists it under Individual components)."
}
$makeappx = Join-Path $sdkBin.FullName 'x64\makeappx.exe'
$makepri = Join-Path $sdkBin.FullName 'x64\makepri.exe'

# --- The build ------------------------------------------------------------

foreach ($name in 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS') {
    if ([Environment]::GetEnvironmentVariable($name)) {
        throw "$name is set, and it replaces the rustflags in .cargo\config.toml, so booth.exe would need the Visual C++ runtime. Clear it: Remove-Item Env:$name"
    }
}
# Out of target\release, so the normal build there is never the Store's.
$target = Join-Path $root 'target\store'
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$buildFolders = @($cargoHome, $root, $target | ForEach-Object { $_.TrimEnd('\', '/') } | Select-Object -Unique)
foreach ($folder in $buildFolders) {
    if ($folder -match '\s') {
        throw "$folder has a space in it, which no C flag can carry, so libopus would write it into booth.exe. Build from folders without spaces."
    }
}
# The same cuts release.ps1 makes, so no folder of this PC goes into
# booth.exe: rustc applies the last prefix that matches and cl.exe the first.
$remaps = foreach ($folder in $buildFolders | Sort-Object Length) {
    '"--remap-path-prefix=' + $folder.Replace('\', '\\') + '="'
}
$buildArgs = @('--config', ('target.x86_64-pc-windows-msvc.rustflags=[' + ($remaps -join ', ') + ']'), '--target-dir', $target)
$trims = foreach ($folder in $buildFolders | Sort-Object Length -Descending) { "/d1trimfile:$folder\" }
$buildEnvironment = @{ CFLAGS = ((@($env:CFLAGS) + $trims) -join ' ').Trim() }

Write-Host "Booth $version for the Microsoft Store, package version $packageVersion"

$out = Join-Path $root 'dist\store'
$layout = Join-Path $out 'layout'
New-Item -ItemType Directory -Force $out | Out-Null
if (Test-Path -LiteralPath $layout) { Remove-Item -LiteralPath $layout -Recurse -Force }
New-Item -ItemType Directory -Force $layout | Out-Null

& (Join-Path $PSScriptRoot 'fetch-third-party.ps1')

Invoke-Tool 'the release tool build' 'cargo' (@('build', '--release', '--locked', '-p', 'release') + $buildArgs) $buildEnvironment
Invoke-Tool 'collecting the third-party licenses' (Join-Path $target 'release\release.exe') @('licenses', $root, $layout)
Invoke-Tool 'the store build' 'cargo' (@('build', '--release', '--locked', '-p', 'app', '--features', 'store') + $buildArgs) $buildEnvironment

Copy-Item -LiteralPath (Join-Path $target 'release\booth.exe') -Destination $layout
Copy-Item -Path (Join-Path $root 'third_party\ffmpeg\bin\*.dll') -Destination $layout
foreach ($file in 'LICENSE-MIT', 'LICENSE-APACHE') {
    Copy-Item -LiteralPath (Join-Path $root $file) -Destination $layout
}

# ffmpeg\SOURCE.txt in the package names each DLL's SHA-256, so the DLLs
# are held to it, as release.ps1 does for the zip.
$ffmpegHashes = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $layout 'ffmpeg\SOURCE.txt')) {
    if ($line -match '^SHA-256 (\S+\.dll): ([0-9A-Fa-f]{64})$') { $ffmpegHashes[$Matches[1]] = $Matches[2] }
}
foreach ($dll in Get-ChildItem -LiteralPath $layout -Filter '*.dll' -File) {
    $got = (Get-FileHash -LiteralPath $dll.FullName -Algorithm SHA256).Hash
    if (-not $ffmpegHashes.ContainsKey($dll.Name) -or $got -ne $ffmpegHashes[$dll.Name]) {
        throw "$($dll.Name) from third_party\ffmpeg\bin has SHA-256 $got, not the one third_party\ffmpeg\SOURCE.txt gives it. Build FFmpeg again with: powershell -ExecutionPolicy Bypass -File tools\build-ffmpeg.ps1"
    }
}

# --- The logos ------------------------------------------------------------

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class StoreLogo {
    // Grid line c of 16 on a side of n pixels. Half rounds down, and the far
    // half mirrors the near one.
    static int Edge(int c, int n) {
        if (c * 2 > 16) return n - Edge(16 - c, n);
        return (int)Math.Ceiling(c * n / 16.0 - 0.5);
    }

    // The mark's rectangles as x0, y0, x1, y1 in pixels of an n pixel square.
    static int[] Fit(int[] grid, int n) {
        int[] r = new int[grid.Length];
        for (int i = 0; i < grid.Length; i += 4) {
            r[i] = Edge(grid[i], n);
            r[i + 1] = Edge(grid[i + 1], n);
            r[i + 2] = Edge(grid[i] + grid[i + 2], n);
            r[i + 3] = Edge(grid[i + 1] + grid[i + 3], n);
        }
        return r;
    }

    static bool On(int[] r, int x, int y) {
        for (int i = 0; i < r.Length; i += 4)
            if (x >= r[i] && x < r[i + 2] && y >= r[i + 1] && y < r[i + 3]) return true;
        return false;
    }

    // The plate's corner is 2/16 of the size, or less where that would cut
    // a corner of the mark: a mark corner a px from one edge and b px from
    // the other stays inside a radius r while r <= a + b + sqrt(2ab).
    static double Radius(int n, int[] r) {
        double radius = n * 2.0 / 16.0, limit = radius;
        for (int i = 0; i < r.Length; i += 4) {
            int[] xs = { r[i], r[i + 2] };
            int[] ys = { r[i + 1], r[i + 3] };
            foreach (int x in xs) foreach (int y in ys) {
                double a = Math.Min(x, n - x), b = Math.Min(y, n - y);
                if (a < radius && b < radius) limit = Math.Min(limit, a + b + Math.Sqrt(2 * a * b));
            }
        }
        return limit;
    }

    // How much of pixel x, y the rounded plate covers, from 64 by 64 samples.
    static double Cover(int n, double radius, int x, int y) {
        bool cornerX = x < radius || x + 1 > n - radius, cornerY = y < radius || y + 1 > n - radius;
        if (!(cornerX && cornerY)) return 1.0;
        const int S = 64;
        int inside = 0;
        for (int sy = 0; sy < S; sy++) {
            for (int sx = 0; sx < S; sx++) {
                double px = x + (sx + 0.5) / S, py = y + (sy + 0.5) / S;
                double cx = Math.Min(Math.Max(px, radius), n - radius), cy = Math.Min(Math.Max(py, radius), n - radius);
                double dx = px - cx, dy = py - cy;
                if (dx * dx + dy * dy <= radius * radius) inside++;
            }
        }
        return inside / (double)(S * S);
    }

    static void Save(uint[] px, int width, int height, string path) {
        using (Bitmap bmp = new Bitmap(width, height, PixelFormat.Format32bppArgb)) {
            BitmapData d = bmp.LockBits(new Rectangle(0, 0, width, height), ImageLockMode.WriteOnly, PixelFormat.Format32bppArgb);
            int[] row = new int[width];
            for (int y = 0; y < height; y++) {
                for (int x = 0; x < width; x++) row[x] = unchecked((int)px[y * width + x]);
                Marshal.Copy(row, 0, d.Scan0 + y * d.Stride, width);
            }
            bmp.UnlockBits(d);
            bmp.Save(path, ImageFormat.Png);
        }
    }

    // The mark on the plate, as in booth.ico.
    public static void Icon(int[] grid, int n, uint plate, uint mark, string path) {
        int[] r = Fit(grid, n);
        double radius = Radius(n, r);
        uint[] px = new uint[n * n];
        for (int y = 0; y < n; y++) {
            for (int x = 0; x < n; x++) {
                uint a = (uint)Math.Round(Cover(n, radius, x, y) * 255.0);
                uint rgb = (On(r, x, y) ? mark : plate) & 0xFFFFFF;
                px[y * n + x] = a == 0 ? 0u : (a << 24) | rgb;
            }
        }
        Save(px, n, n, path);
    }

    // A full background with the mark on a box side by side pixels square in
    // the middle. Windows draws the tile's own corners.
    public static void Tile(int[] grid, int width, int height, int side, uint plate, uint mark, string path) {
        int[] r = Fit(grid, side);
        int ox = (width - side) / 2, oy = (height - side) / 2;
        uint[] px = new uint[width * height];
        for (int y = 0; y < height; y++)
            for (int x = 0; x < width; x++)
                px[y * width + x] = On(r, x - ox, y - oy) ? mark : plate;
        Save(px, width, height, path);
    }
}
'@

# docs\mark.svg: a 16 by 16 grid of filled rectangles, each one closed
# subpath of M, h, v, H and V.
$svg = [xml](Get-Content -Raw -LiteralPath (Join-Path $root 'docs\mark.svg'))
if ($svg.svg.viewBox -ne '0 0 16 16') {
    throw "docs\mark.svg has the viewBox $($svg.svg.viewBox); the logos are drawn on its 16 by 16 grid."
}
$markColour = [Convert]::ToUInt32('FF' + $svg.svg.path.fill.TrimStart('#'), 16)
$panelColour = [Convert]::ToUInt32('FF1E1E1B', 16)
$grid = New-Object System.Collections.Generic.List[int]
foreach ($subpath in [regex]::Matches($svg.svg.path.d, '[Mm][^Mm]*')) {
    $x = 0; $y = 0; $xs = @(); $ys = @()
    foreach ($step in [regex]::Matches($subpath.Value, '([MmHhVvZz])\s*(-?\d+)?(?:[\s,]+(-?\d+))?')) {
        $command = $step.Groups[1].Value
        $a = if ($step.Groups[2].Success) { [int]$step.Groups[2].Value } else { 0 }
        switch -CaseSensitive ($command) {
            'M' { $x = $a; $y = [int]$step.Groups[3].Value }
            'H' { $x = $a }
            'h' { $x += $a }
            'V' { $y = $a }
            'v' { $y += $a }
            { $_ -in 'Z', 'z' } { }
            default { throw "docs\mark.svg uses the path command $command, and the logos know only M, h, v, H, V and z." }
        }
        $xs += $x; $ys += $y
    }
    $x0 = ($xs | Measure-Object -Minimum).Minimum; $x1 = ($xs | Measure-Object -Maximum).Maximum
    $y0 = ($ys | Measure-Object -Minimum).Minimum; $y1 = ($ys | Measure-Object -Maximum).Maximum
    $grid.AddRange([int[]]@($x0, $y0, ($x1 - $x0), ($y1 - $y0)))
}
$grid = $grid.ToArray()

$assets = Join-Path $layout 'Assets'
New-Item -ItemType Directory -Force $assets | Out-Null
# Half up, as the Store's own size tables have it: StoreLogo at 125% is 63.
function Get-Scaled([int]$Size, [double]$K) { [int][math]::Round($Size * $K, [MidpointRounding]::AwayFromZero) }
foreach ($scale in 100, 125, 150, 200, 400) {
    $k = $scale / 100
    [StoreLogo]::Icon($grid, (Get-Scaled 44 $k), $panelColour, $markColour, (Join-Path $assets "Square44x44Logo.scale-$scale.png"))
    [StoreLogo]::Icon($grid, (Get-Scaled 50 $k), $panelColour, $markColour, (Join-Path $assets "StoreLogo.scale-$scale.png"))
    # The mark on a whole number of pixels per grid cell, about half the
    # tile's height.
    $height = Get-Scaled 150 $k
    $side = 16 * [int][math]::Round($height / 32)
    [StoreLogo]::Tile($grid, $height, $height, $side, $panelColour, $markColour, (Join-Path $assets "Square150x150Logo.scale-$scale.png"))
    [StoreLogo]::Tile($grid, (Get-Scaled 310 $k), $height, $side, $panelColour, $markColour, (Join-Path $assets "Wide310x150Logo.scale-$scale.png"))
}
# The taskbar, Start's list and Explorer pick these by pixel size. The
# plate is in the picture, so the unplated ones, which Windows shows as they
# are, are the same pictures.
foreach ($size in 16, 24, 32, 48, 256) {
    $file = Join-Path $assets "Square44x44Logo.targetsize-$size.png"
    [StoreLogo]::Icon($grid, $size, $panelColour, $markColour, $file)
    Copy-Item -LiteralPath $file -Destination (Join-Path $assets "Square44x44Logo.targetsize-${size}_altform-unplated.png")
}

# --- The manifest and the resource index ----------------------------------

$manifest = Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'store\AppxManifest.xml')
$values = @{
    Name = $identity.Name
    Publisher = $identity.Publisher
    PublisherDisplayName = $identity.PublisherDisplayName
    Version = $packageVersion
}
foreach ($key in $values.Keys) {
    $manifest = $manifest.Replace("{{$key}}", [System.Security.SecurityElement]::Escape($values[$key]))
}
if ($manifest -match '\{\{(\w+)\}\}') {
    throw "tools\store\AppxManifest.xml asks for {{$($Matches[1])}}, which store.ps1 does not fill in."
}
Write-Utf8 (Join-Path $layout 'AppxManifest.xml') $manifest

# Windows finds Assets\Square44x44Logo.png and the rest through
# resources.pri, which maps each name to its scale and size variants.
# makepri indexes a folder with only the manifest and the logos, so the
# licenses and binaries do not become resources.
$priRoot = Join-Path $out 'pri'
if (Test-Path -LiteralPath $priRoot) { Remove-Item -LiteralPath $priRoot -Recurse -Force }
New-Item -ItemType Directory -Force $priRoot | Out-Null
Copy-Item -LiteralPath $assets -Destination $priRoot -Recurse
Copy-Item -LiteralPath (Join-Path $layout 'AppxManifest.xml') -Destination $priRoot
$priConfig = Join-Path $out 'priconfig.xml'
Invoke-Tool 'makepri createconfig' $makepri @('createconfig', '/cf', $priConfig, '/dq', 'en-US', '/pv', '10.0.0', '/o') -Quiet
# The config makepri writes splits the scales into resource packs, which
# only a bundle has. One package keeps them all in one resources.pri.
$config = [xml](Get-Content -Raw -LiteralPath $priConfig)
foreach ($packaging in @($config.SelectNodes('//packaging'))) { [void]$packaging.ParentNode.RemoveChild($packaging) }
$config.Save($priConfig)
Invoke-Tool 'makepri new' $makepri @('new', '/pr', $priRoot, '/cf', $priConfig, '/mn', (Join-Path $priRoot 'AppxManifest.xml'), '/of', (Join-Path $layout 'resources.pri'), '/o') -Quiet
Remove-Item -LiteralPath $priRoot -Recurse -Force
Remove-Item -LiteralPath $priConfig -Force

# --- This PC's folders, which nothing in the package may hold -------------

$latin1 = [System.Text.Encoding]::GetEncoding(28591)
$options = [System.Text.RegularExpressions.RegexOptions]'IgnoreCase, CultureInvariant'
$needles = @(New-Object regex ':[\\/]+Users\b', $options) + @(
    $env:USERPROFILE, $cargoHome, $root, $target | Where-Object { $_ } | ForEach-Object {
        $parts = @($_ -split '[\\/]+' | Where-Object { $_ }) | ForEach-Object { [regex]::Escape($_) }
        New-Object regex ($parts -join '[\\/]+'), $options
    })
$hits = @()
foreach ($file in Get-ChildItem -LiteralPath $layout -Recurse -File) {
    $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
    $texts = @($latin1.GetString($bytes))
    if ($bytes.Length -ge 2) {
        $texts += [System.Text.Encoding]::Unicode.GetString($bytes)
        $texts += [System.Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)
    }
    foreach ($text in $texts) {
        foreach ($needle in $needles) {
            $found = $needle.Match($text)
            if ($found.Success) { $hits += "$($file.FullName) holds $($found.Value)" }
        }
    }
}
if ($hits) {
    throw ((@('The package would carry folders of this PC, so it stops here.') + ($hits | Select-Object -Unique)) -join "`n")
}

# --- The package ----------------------------------------------------------

$package = Join-Path $out $packageName
if (Test-Path -LiteralPath $package) { Remove-Item -LiteralPath $package -Force }
Invoke-Tool 'makeappx pack' $makeappx @('pack', '/d', $layout, '/p', $package, '/o') -Quiet
Remove-Item -LiteralPath $layout -Recurse -Force

Write-Host ''
Write-Host "$package, $('{0:N1} MB' -f ((Get-Item -LiteralPath $package).Length / 1MB)), unsigned"
if ($Test) {
    Write-Host 'For this PC only. Sign it with a certificate whose subject is CN=Booth local test, trusted under Local Machine, Trusted People, before Add-AppxPackage takes it.'
} else {
    Write-Host 'Upload it in Partner Center under Packages. The Store signs it.'
}
