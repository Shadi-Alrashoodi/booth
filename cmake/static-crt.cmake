# booth.exe links the C runtime statically (.cargo/config.toml), so C code
# built with CMake must too, or the linker mixes the two runtimes. libopus
# picks its runtime from its own switch and ignores the first line.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded")
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "")

# Under the Visual Studio generator the cmake crate replaces
# CMAKE_C_FLAGS_RELEASE and drops any /O flag, so CMake's /O2 goes and cl.exe
# falls back to no optimisation at all.
add_compile_options("$<$<CONFIG:Release>:/O2>")
