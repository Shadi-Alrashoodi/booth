#!/bin/sh
# The part of tools\build-ffmpeg.ps1 that runs in MSYS2's bash: FFmpeg's
# configure and make need a POSIX shell, while the compiler is MSVC from the
# Visual Studio x64 environment the script hands down.
#
#   build.sh <unpacked FFmpeg source> <folder to install into> <log file>
#
# Only what crates\decode uses: avcodec and avutil as DLLs, the H.264 and HEVC
# decoders with their D3D11 hardware paths, and Windows' own threads. Each
# hardware path is two hwaccels in FFmpeg: d3d11va offers the D3D11 format
# to the format callback and d3d11va2 decodes into it. --disable-faan drops
# the floating point DCTs, which neither decoder here uses, and with them
# three files under the libjpeg and ISC licenses.
#
# No outside library, no --enable-gpl, --enable-version3 or --enable-nonfree,
# so the DLLs are LGPL version 2.1 or later. -MT links the C runtime into each
# DLL, so they need no Visual C++ redistributable, like booth.exe. -Brepro
# makes cl and link write the same bytes for the same input instead of
# stamping the time in.
set -eu

export PATH="/usr/bin:$PATH"
source=$(cygpath -u "$1")
install=$(cygpath -u "$2")
exec >"$(cygpath -u "$3")" 2>&1

# The build runs inside the source folder, so the paths the compiler is
# given are relative. A header found beside the file that includes it still
# goes into __FILE__ with its full path, and FFmpeg keeps a few of those in
# the DLLs. /d1trimfile, an MSVC option Microsoft does not document, cuts
# the build folder off them; it goes through CL and not configure so the
# configure line the DLLs carry names no folder either. The same source then
# builds the same bytes in any folder.
export CL="/d1trimfile:$(cygpath -w "$source")\\"
# configure's dependency step picks the headers out of cl's -showIncludes
# lines by their English wording, so cl answers in English whatever
# language Visual Studio is installed in.
export VSLANG=1033

cd "$source"
./configure \
    --toolchain=msvc \
    --arch=x86_64 \
    --prefix=/ffmpeg \
    --enable-shared \
    --disable-static \
    --disable-autodetect \
    --disable-everything \
    --disable-programs \
    --disable-doc \
    --disable-network \
    --disable-avdevice \
    --disable-avformat \
    --disable-avfilter \
    --disable-swscale \
    --disable-swresample \
    --disable-faan \
    --disable-debug \
    --enable-w32threads \
    --enable-d3d11va \
    --enable-decoder=h264,hevc \
    --enable-hwaccel=h264_d3d11va,h264_d3d11va2,hevc_d3d11va,hevc_d3d11va2 \
    --extra-cflags=-MT \
    --extra-cflags=-Brepro \
    --extra-ldflags=-Brepro

make -j"$(nproc)"
make install DESTDIR="$install"

# Most of FFmpeg is under the LGPL, but a few files compiled into these DLLs
# carry a permissive license of their own, whose notice has to go with the
# DLLs. These are found, not listed by hand: every source and header the
# compiler read for the two libraries, from each object's source and
# dependency file, whose first lines do not name the LGPL. Files configure
# generates and files that only include another are left out, and so are
# the compiler's and the Windows SDK's headers, the paths still absolute
# once the source folder is cut off.
here=$(cygpath -m "$source")
for object in $(find libavcodec libavutil -name '*.o'); do
    base=${object%.o}
    for file in "$base.c" "$base.asm"; do
        if [ -f "$file" ]; then echo "$file"; fi
    done
    if [ -f "$base.d" ]; then
        sed -e 's/^[^:]*: *//' -e 's/\\$//' "$base.d" | tr ' ' '\n'
    fi
done | sed -e "s#^$here/##" -e 's#^\./##' | grep -v -E '^$|^[A-Za-z]:|^/' | sort -u >compiled.txt
# With no header in the list, a header's notice would be missed without a
# word.
if ! grep -q '\.h$' compiled.txt; then
    echo "the dependency files name no FFmpeg header, so the notices of headers under other licenses cannot be collected. cl may be answering in a language other than English: add the English language pack in the Visual Studio Installer."
    exit 1
fi

notices="$install/ffmpeg/NOTICES.txt"
{
    echo "Files compiled into avcodec and avutil that are under a license of their own"
    echo "rather than the LGPL, each with the notice at its top in FFmpeg's source:"
} >"$notices"
count=0
while read -r file; do
    case "$file" in
    config.h | config.asm | config_components.h | config_components.asm | \
        libavutil/avconfig.h | libavutil/ffversion.h | libavcodec/*_list.c) continue ;;
    esac
    if head -n 40 "$file" | grep -q 'Lesser General Public'; then continue; fi
    case "$file" in
    *.asm) notice=$(awk '!/^;/ { exit } { print }' "$file") ;;
    *) notice=$(awk 'NR == 1 && !/^\/\*/ { exit } { print } /\*\// { exit }' "$file") ;;
    esac
    if [ -z "$notice" ]; then continue; fi
    printf '\n%s\n\n%s\n' "$file" "$notice" >>"$notices"
    # The IJG's condition for code shipped without its source, which the zip
    # is.
    case "$notice" in
    *"Independent JPEG Group"*)
        printf '\n%s\n' "This software is based in part on the work of the Independent JPEG Group." >>"$notices"
        ;;
    esac
    count=$((count + 1))
done <compiled.txt
echo "notices of $count files under other licenses are in $notices"
