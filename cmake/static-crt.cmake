# booth.exe links the C runtime statically (.cargo/config.toml), so C code
# built with CMake must too, or the linker mixes the two runtimes. libopus
# picks its runtime from its own switch and ignores the first line.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded")
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "")
