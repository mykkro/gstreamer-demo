# Dot-source before building/running the Rust demo:   . .\env.ps1
# Points cargo (pkg-config) and the binaries (DLLs) at the GStreamer MSVC SDK.
$root = $env:GSTREAMER_1_0_ROOT_MSVC_X86_64
if (-not $root) {
    foreach ($c in "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64", "$env:ProgramFiles\gstreamer\1.0\msvc_x86_64", "C:\gstreamer\1.0\msvc_x86_64") {
        if (Test-Path "$c\bin\pkg-config.exe") { $root = $c; break }
    }
}
if (-not $root -or -not (Test-Path "$root\lib\pkgconfig\gstreamer-1.0.pc")) {
    Write-Error "GStreamer MSVC SDK (devel) not found. Install it from https://gstreamer.freedesktop.org/download/ with the 'devel' type."
    return
}
$root = $root.TrimEnd('\')
$env:GSTREAMER_1_0_ROOT_MSVC_X86_64 = "$root\"
$env:PKG_CONFIG = "$root\bin\pkg-config.exe"
$env:PKG_CONFIG_PATH = "$root\lib\pkgconfig"
if (-not ($env:PATH -split ';' -contains "$root\bin")) { $env:PATH = "$root\bin;$env:PATH" }
Write-Host "GStreamer SDK: $root"
