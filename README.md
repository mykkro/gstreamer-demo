# GStreamer H.265 producer / receiver demo (Python + Rust, Windows)

One **producer** decodes a video once (a looped `.mp4` or a built-in test pattern) and offers it as several
**H.265 channels** (different resolution / fps / bitrate). Any number of **receivers** ask the producer
what channels exist, pick one, and display it.

There are two implementations of the same design, **Python** ([python/](python/)) and **Rust**
([rust/](rust/)). They use the same ports, pipeline, and JSON control protocol, so you can mix them freely:
a Rust producer with Python receivers, or the other way round.

```
gstreamer-demo/
├── README.md
├── python/            producer.py, receiver.py, common.py, requirements.txt
├── rust/              Cargo.toml, env.ps1, src/lib.rs, src/bin/{producer,receiver}.rs
└── .venv/             Python virtual env (created in step 1)
```

```
                         ┌────────────────── producer ────────────────────────────────────────┐
 sample.mp4 (looped) ──► │ decode ─► tee ─┬─► valve ─► scale ─► H.265 ─► MPEG-TS ─► TCP :5001  │──► receiver A (1080p30)
   or test pattern       │                ├─► valve ─► scale ─► H.265 ─► MPEG-TS ─► TCP :5002  │──► receiver B, C (720p30)
                         │                └─► valve ─► scale ─► H.265 ─► MPEG-TS ─► TCP :5003  │
                         │ control server  TCP :5000   {"cmd":"list"} → channel list           │◄── receivers ask first
                         └─────────────────────────────────────────────────────────────────────┘
```

## 1. Python: install prerequisites (Windows 10/11)

Since GStreamer 1.28 there are official Python wheels on PyPI. They include the GStreamer runtime, all
plugins (also x265, NVENC, libav), the `gst-launch-1.0` tools, and the Python bindings (`gi`). You don't
need the MSI installer, MSYS2, or to build anything yourself.

1. Install **Python 3.9 – 3.14** (64-bit) from python.org.
2. In the repository root:

   ```powershell
   python -m venv .venv
   .\.venv\Scripts\Activate.ps1          # cmd.exe: .venv\Scripts\activate.bat
   pip install --upgrade pip
   pip install -r python\requirements.txt   # gstreamer-bundle, opencv-python, numpy
   ```

3. Check it:

   ```powershell
   python -c "import gi; gi.require_version('Gst','1.0'); from gi.repository import Gst; Gst.init(None); print(Gst.version_string())"
   gst-inspect-1.0 nvh265enc   # NVIDIA GPU encoder (only listed if an NVIDIA GPU + driver is present)
   gst-inspect-1.0 x265enc     # CPU encoder, always available
   ```

> **About `opencv-python`:** the pip wheel of OpenCV for Windows is built **without** GStreamer support
> (`cv2.getBuildInformation()` shows `GStreamer: NO`), so `cv2.VideoCapture("... ! appsink", cv2.CAP_GSTREAMER)`
> will not work. This demo therefore lets GStreamer (via `gi`) do networking and decoding, and hands decoded
> frames to OpenCV as numpy arrays. OpenCV is used for display and any processing you want to add. To use
> `CAP_GSTREAMER` directly, you'd have to build OpenCV from source against GStreamer, which isn't worth it here.

The wheels are large (several hundred MB). If you prefer the classic route, the MSVC
installer from <https://gstreamer.freedesktop.org/download/> also works, but you then have to make Python
find the `gi` bindings yourself.

## 2. Python: run

```powershell
# terminal 1 – producer with a test pattern (no file needed) …
python python\producer.py
# … or loop an mp4
python python\producer.py C:\videos\movie.mp4

# terminal 2, 3, … – receivers
python python
eceiver.py                      # lists channels, asks which one
python python
eceiver.py --channel 720p30     # subscribe directly
python python
eceiver.py --host 192.168.1.10  # producer on another machine (allow ports 5000-5003 in the firewall)
python python
eceiver.py --list               # just print what the producer offers
```

In the receiver window press **1..9** to switch channel and **q** / **Esc** to quit.

To make a quick test clip:

```powershell
gst-launch-1.0 videotestsrc num-buffers=300 pattern=ball ! video/x-raw,width=1280,height=720,framerate=30/1 ! timeoverlay ! x264enc ! h264parse ! mp4mux ! filesink location=sample.mp4
```

### Producer options

| option | default | meaning |
|---|---|---|
| `source` | `test` | video file to loop, or `test` |
| `--channels` | `1080p30:4000,720p30:2000,480p15:600` | `<height>p<fps>[:kbps]`, 16:9 width is derived |
| `--encoder` | `auto` | `auto` (nvenc → qsv → amf → x265), `x265`, `nvenc`, `qsv`, `amf`, `mf`, or any GStreamer element name |
| `--gop-seconds` | `2` | keyframe interval; also the worst-case join time for a new subscriber |
| `--always-encode` | off | encode every channel even when nobody watches |
| `--control-port` / `--base-port` | `5000` / `5001` | channel *i* uses `base-port + i` |

### Receiver options

| option | default | meaning |
|---|---|---|
| `--decoder` | `auto` | `auto` (decodebin: picks the D3D12/D3D11 hardware decoder), `sw` (libav), `d3d11`, `d3d12`, `nvdec` |
| `--display` | `opencv` | `opencv` = frames into Python; `gst` = native GStreamer window (lowest CPU) |
| `--no-window --duration 10` | | headless benchmark: decode and print fps |


## 3. Rust: install, build, run

The Rust version uses [gstreamer-rs](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs) (`gstreamer`, `gstreamer-video`
crates). Building needs the GStreamer **development** files (headers, `.lib` import libraries, `pkg-config`).
The pip wheels don't include those, so install the official SDK:

1. **Rust** with the MSVC toolchain: <https://rustup.rs> (`stable-x86_64-pc-windows-msvc`). This also
   requires the *Visual Studio Build Tools* with the "Desktop development with C++" workload.
2. **GStreamer MSVC SDK 1.28.x**: download `gstreamer-1.0-msvc-x86_64-<version>.exe` from
   <https://gstreamer.freedesktop.org/download/> and choose the **Development** install type, or install
   silently for your user only:

   ```powershell
   .\gstreamer-1.0-msvc-x86_64-1.28.7.exe /VERYSILENT /CURRENTUSER /TYPE=devel
   ```

   A user-only install goes to `%LOCALAPPDATA%\Programs\gstreamer\1.0\msvc_x86_64`, and a system-wide one
   to `%ProgramFiles%\gstreamer\1.0\msvc_x86_64`.
3. Set up the shell. The installer does **not** put the SDK on `PATH`. The env script finds the SDK and sets
   `PKG_CONFIG`, `PKG_CONFIG_PATH` (needed by `cargo build`), and `PATH` (needed to find the DLLs when the `.exe`
   runs), **for the current window only**. Use the script that matches your shell:

   ```bat
   :: cmd.exe   (prompt looks like  C:\Work\gstreamer-demo\rust>)
   cd rust
   .\env.cmd
   cargo build --release
   ```

   ```powershell
   # PowerShell (prompt looks like  PS C:\Work\gstreamer-demo\rust>)
   cd rust
   . .\env.ps1                       # note the leading dot: "dot-source" it into this shell
   cargo build --release
   ```

   An activated Python `.venv` doesn't matter here and can stay active.

   *Optional, permanent:* to skip the script in every new window, add the variables to your user environment
   once (PowerShell, then open a **new** terminal):

   ```powershell
   $r = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"
   [Environment]::SetEnvironmentVariable('GSTREAMER_1_0_ROOT_MSVC_X86_64', "$r\", 'User')
   [Environment]::SetEnvironmentVariable('PKG_CONFIG', "$r\bin\pkg-config.exe", 'User')
   [Environment]::SetEnvironmentVariable('PKG_CONFIG_PATH', "$r\lib\pkgconfig", 'User')
   [Environment]::SetEnvironmentVariable('Path', "$r\bin;" + [Environment]::GetEnvironmentVariable('Path','User'), 'User')
   ```

4. Run (in a window where the env script ran, so the GStreamer DLLs are found):

   ```powershell
   .\target\release\producer.exe                         # test pattern
   .\target\release\producer.exe ..\sample.mp4           # loop a file
   .\target\release\receiver.exe --channel 720p30
   .\target\release\receiver.exe --list
   # or: cargo run --release --bin receiver -- --channel 720p30
   ```

The command-line options are the same as the Python scripts (`--channels`, `--encoder`, `--gop-seconds`,
`--always-encode`, `--host`, `--decoder`, `--no-window`, `--duration`, …). See `--help`.

**Differences from the Python receiver:** the Rust receiver doesn't use OpenCV. Decoded frames go straight
into GStreamer's own video window (D3D12/D3D11), which is the most efficient path. A `textoverlay` shows
the channel and the measured fps. Keys **1..9** and **q** / **Esc** arrive as GStreamer *navigation* events
from that window. If you need frames in Rust code, replace the sink with `appsink` (crate `gstreamer-app`).
For OpenCV in Rust, see the [`opencv`](https://crates.io/crates/opencv) crate, which needs a separate
OpenCV + LLVM/libclang install.

> **Antivirus:** some AV products (e.g. Avast) quarantine freshly built `build-script-build.exe` / binaries
> in `rust\target`. If `cargo build` fails with "access denied" or files that vanish, add `rust\target` to the
> AV exceptions.

## 4. Control protocol

Newline-delimited JSON over TCP (port 5000). Both implementations speak it ([python/common.py](python/common.py), [rust/src/lib.rs](rust/src/lib.rs)):

```json
→ {"cmd": "list"}
← {"ok": true, "source": "test", "channels": [
     {"id": "720p30", "width": 1280, "height": 720, "fps": 30, "bitrate_kbps": 2000,
      "port": 5002, "codec": "h265", "container": "mpegts", "transport": "tcp", "encoder": "nvh265enc"}, ...]}
→ {"cmd": "stats"}
← {"ok": true, "clients": {"1080p30": 0, "720p30": 2, "480p15": 1}}
```

## 5. How it scales to many subscribers

- **Decode once, encode once per channel.** The source is decoded once and split with `tee`. Each channel
  is encoded once. `tcpserversink` then sends the same encoded bytes to every connected client. Ten viewers
  of `720p30` cost one encoder plus network bandwidth, not ten encoders.
- **Encode on demand.** Each channel has a `valve` that stays closed until its first subscriber connects
  and closes again when the last one leaves. Channels nobody watches cost no scaling or encoding.
- **Fast join.** When a client connects, the producer asks the encoder for an immediate IDR frame
  (force-key-unit event). `h265parse config-interval=-1` re-sends VPS/SPS/PPS before every keyframe, so a
  late joiner can start decoding right away instead of waiting for the next GOP.
  `sync-method=next-keyframe` makes sure each new client's stream starts at a keyframe.
- **Slow clients don't stall others.** `tcpserversink` keeps a separate queue per client. Clients that
  fall too far behind are resynced to a keyframe (`recover-policy=keyframe`).
- **Seamless file loop.** The mp4 is played with *segment seeks*. Instead of EOS, the demuxer posts
  `SEGMENT_DONE` and the producer seeks back to 0 without flushing. Timestamps keep increasing, so
  receivers never see the stream end.

## 6. H.265 notes and optimization

**Encoders available on Windows** (`gst-inspect-1.0 | findstr h265enc`):

| element | runs on | notes |
|---|---|---|
| `nvh265enc` | NVIDIA NVENC | best choice if present; near-zero CPU. Consumer GeForce cards allow a limited number of simultaneous encode sessions (currently 8), and one channel = one session |
| `qsvh265enc` | Intel Quick Sync | iGPU / Arc |
| `amfh265enc` | AMD AMF | Radeon |
| `mfh265enc` | Media Foundation | uses whatever vendor driver is installed; fewer knobs |
| `x265enc` | CPU (x265) | always available, best quality per bit at slow presets, but expensive: 1080p30 real-time needs `ultrafast`/`superfast` |

**Settings used for low latency** (see `encoder_props()` in [python/producer.py](python/producer.py) and [rust/src/bin/producer.rs](rust/src/bin/producer.rs)):

- No B-frames, no lookahead (`tune=zerolatency` for x265, `zerolatency=true`, `tune=low-latency` for NVENC).
  B-frames save about 10–20 % bitrate but add frames of latency.
- CBR at a fixed bitrate, which is predictable for networks.
- GOP = 2 s. A shorter GOP gives faster join and recovery but more bitrate. A longer GOP is the opposite.
  Joins are fast anyway because of force-key-unit.

**Rough bitrates for H.265 at low latency** (about 40–50 % less than H.264 at similar quality):
480p15 ≈ 0.4–0.8 Mbps · 720p30 ≈ 1.5–2.5 Mbps · 1080p30 ≈ 3–5 Mbps · 4K30 ≈ 10–16 Mbps.
Static content (screens, cameras) needs much less than motion-heavy content.

**Further optimizations**

- *Receiver:* `--decoder auto` already uses the GPU decoder (D3D12/D3D11). With `--display gst` the frames
  never go through Python. Frames only need to come to CPU/numpy when you actually process them in OpenCV.
- *Producer scaling:* `videoscale` / `videoconvert` run on the CPU. For 4K sources or many channels, keep
  frames on the GPU (`d3d11convert` / `d3d12convert`, or `cudaconvertscale` with NVENC).
- *x265 quality vs. CPU:* `speed-preset` `ultrafast` → `superfast` → `veryfast` … each step costs roughly
  1.5–2× the CPU for a few % lower bitrate. Extra x265 parameters go in `option-string`, e.g.
  `"aq-mode=2:rc-lookahead=0"`.
- *Many receivers on a LAN:* replace TCP with **RTP over UDP multicast**
  (`rtph265pay config-interval=-1 ! udpsink host=239.1.1.1 auto-multicast=true`). The producer then sends
  each packet once, however many receivers there are, but you lose TCP's reliability.
- *Internet / NAT / players like VLC:* use **RTSP** (`gst-rtsp-server`, also in the wheels), **SRT**
  (`srtsink`, good on lossy links), or **WebRTC** (`webrtcsink`) for browsers.
  Browser support for H.265 varies, so check your target browsers before relying on it.

## 7. Files

| file | purpose |
|---|---|
| [python/producer.py](python/producer.py) | source → tee → per-channel H.265 encode → TCP servers, plus the control server |
| [python/receiver.py](python/receiver.py) | queries channels, subscribes, decodes, shows frames with OpenCV |
| [python/common.py](python/common.py) | GStreamer init + the JSON control request helper |
| [python/requirements.txt](python/requirements.txt) | `gstreamer-bundle`, `opencv-python`, `numpy` |
| [rust/src/bin/producer.rs](rust/src/bin/producer.rs) | Rust port of the producer (same pipeline and protocol) |
| [rust/src/bin/receiver.rs](rust/src/bin/receiver.rs) | Rust receiver, native GStreamer video window |
| [rust/src/lib.rs](rust/src/lib.rs) | shared `Channel` type + control request helper |
| [rust/env.cmd](rust/env.cmd), [rust/env.ps1](rust/env.ps1) | set up the shell for the GStreamer SDK before `cargo build` / running (cmd.exe / PowerShell) |

## Troubleshooting

- **`ImportError: No module named gi`**: you're not in the venv where `gstreamer-bundle` is installed.
- **Receiver says "Cannot reach producer"**: start the producer first. If it runs on another PC, allow
  TCP 5000–5003 in Windows Defender Firewall.
- **Rust: `The pkg-config command could not be found` / `PKG_CONFIG_PATH=` is empty when building**: the env
  script didn't run in this window. In cmd.exe run `.\env.cmd`. In PowerShell, dot-source `. .\env.ps1`.
  Running `env.ps1` from cmd.exe, or without the leading dot, does nothing to the current shell. If it still
  fails, the SDK may have been installed as *Runtime* only. Re-run the installer with the *Development* type.
- **Rust: `STATUS_DLL_NOT_FOUND` (0xc0000135) when running**: the SDK `bin` folder isn't on `PATH`. Run
  the env script in that window first (or set the variables permanently, see section 3).
- **Debug logging:** `$env:GST_DEBUG="3"` (or `"tcpserversink:5"`) before starting a script.
- **High CPU on the producer:** you're probably on `x265enc`. Try `--encoder nvenc`/`qsv`/`amf`, lower the
  channel list, or use `--channels 720p30:2000`.
