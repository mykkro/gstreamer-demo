# bevy-streamer: streaming camera views from a Bevy 0.18 game

The same idea as [sim-streamer](../sim-streamer/), a car with six cameras driving through a synthetic city,
but built as a normal **Bevy 0.18.1** app: ECS entities, PBR materials, a shadow-casting sun, fog, and a
camera rig parented to the car. Compared with sim-streamer's single atlas image:

- **Each camera renders into its own image**, with its own resolution and frame rate, and is streamed as its
  own H.265 channel.
- **Unwatched cameras aren't rendered at all.** A camera only renders while someone subscribes to its channel
  or to the mosaic.
- A **mosaic** channel is still offered. GStreamer's `compositor` builds it from the individual camera streams.

![mosaic channel: cameras with different resolutions and rates, composited by GStreamer](docs/mosaic.png)

![front channel at 1280x720](docs/front.png)

## How it's done in Bevy

You render to a texture. Bevy has everything built in, with no custom render-graph node:

| step | Bevy API | in this demo |
|---|---|---|
| 1. run **headless** | `WindowPlugin { primary_window: None, exit_condition: DontExit }`, `.disable::<WinitPlugin>()`, `ScheduleRunnerPlugin::run_loop(1/fps)` | the app ticks at the *fastest* camera's rate |
| 2. **one render-target image per camera** | `Image::new_target_texture(w, h, Rgba8UnormSrgb, None)` + `TextureUsages::COPY_SRC` | each camera has its own size, e.g. front 1280×720, top 512×512 |
| 3. **camera → its image** | `RenderTarget::Image(handle.into())` component on the `Camera3d` | six cameras, children of the car entity |
| 4. **GPU → CPU** | entity with `Readback::texture(handle)` + `.observe(\|ev: On<ReadbackComplete>\| …)` | the observer pushes the bytes into *that camera's* `appsrc` |
| 5. **render only what's needed** | `Camera::is_active` + inserting/removing the `Readback` component each tick | see below |
| 6. **→ GStreamer** | `gst_demo::streams::CameraStreams` | one `appsrc` → H.265 channel per camera, plus the compositor mosaic |

### Per-camera rates and on-demand rendering

Every tick the `schedule_cameras` system decides, for each camera, whether it renders on this tick:

```
wanted = own channel has subscribers || mosaic has subscribers   // CameraStreams::demand()
due    = tick % (app_fps / camera_fps) == 0                       // 15 fps camera: every 2nd tick of 30
active = wanted && due
camera.is_active = active
if active { insert Readback::texture(image) } else { remove::<Readback>() }
```

- **Subscriber counts** come from the `tcpserversink`s (`num-handles`). `demand()` also opens and closes the
  mosaic feeds, so nothing is scaled or composited for a mosaic nobody watches.
- **Readback only on render ticks.** A `Readback` component reads its texture *every* frame. If it stayed
  attached while the camera was inactive, the stale image would be streamed again. Attaching it only on ticks
  where the camera renders makes sure each rendered frame is streamed exactly once. Camera and readback are
  extracted to the render world in the same frame, so they stay consistent.
- **What idle costs:** an inactive camera costs nothing in the render world: no culling, no shadow cascades,
  no draw calls, no readback. The simulation itself keeps running.

Measured with the default rig (GTX 1050):

| who is watching | cameras rendering (frames/s rendered and streamed) |
|---|---|
| nobody | none (`front idle \| rear idle \| … \| top idle`) |
| `rear` + `top` | rear 14.7/15, top 9.7/10, the other four idle |
| `mosaic` | front 29.6/30, rear/left/right 14.8/15, chase 29.6/30, top 9.8/10; mosaic arrives at 29.3 fps |

The console prints this table every 5 s.

### The mosaic with mixed resolutions and rates

Each camera's `tee` has a second branch: `valve → videoscale → tile size → compositor`. The live
`compositor` (`force-live=true ignore-inactive-pads=true`) produces mosaic frames at the app rate from the
latest frame of each camera. Slower cameras simply repeat, and cameras with a different aspect ratio (the
square `top` view) are letterboxed.

### Other Bevy details

- **Deterministic time.** `TimeUpdateStrategy::ManualDuration(1/app_fps)` makes `Time` advance exactly one
  tick per update. `ScheduleRunnerPlugin` does the real-time pacing.
- **Camera rig** = children of the car entity (`commands.entity(ego).add_child(camera)`). Poses are in the car's
  local frame, where Bevy's forward is −Z.
- **Readback rows** are padded to 256 bytes. Camera widths must be multiples of 64, so there is no padding; the
  observer would strip it anyway.
- **Readback is asynchronous.** The bytes arrive a frame or two after rendering, which is fine for streaming.
  For exact per-frame metadata (pose, sim time), pair frames with a tick counter.
- **Ctrl+C:** Bevy's `TerminalCtrlCHandlerPlugin` is disabled because the `ctrlc` crate allows only one
  handler. The GStreamer thread handles Ctrl+C and tells Bevy to exit (`AppExit`).
- **Threads:** Bevy owns the main thread, and the GStreamer/GLib main loop runs on a second thread.

## Run

Prerequisites are the same as the other Rust demos (GStreamer MSVC SDK *devel*, `env.cmd` / `env.ps1`).
Bevy is **not** in the workspace's default members because its first build takes several minutes:

```bat
cd rust
.\env.cmd
cargo build --release -p bevy-streamer      :: first build ≈ 5 min, later ones ≈ 1–2 min
target\release\bevy-streamer.exe
```

Default channels:

| channel | resolution | fps | kbps | port |
|---|---|---|---|---|
| `mosaic` | 1920×720 (3×2 tiles of 640×360) | 30 | 5000 | 5001 |
| `front` | 1280×720 | 30 | 2500 | 5002 |
| `rear`, `left`, `right` | 640×360 | 15 | 700 | 5003–5005 |
| `chase` | 960×540 | 30 | 1800 | 5006 |
| `top` | 512×512 | 10 | 600 | 5007 |

Watch it like any producer: `target\release\receiver.exe --channel front`, or in the browser with
`node server.js` in `web/h265-webcodecs`, then <http://localhost:8080/?channel=mosaic>.

Options:

| option | meaning |
|---|---|
| `--camera NAME=WxH@FPS[:KBPS]` (repeatable) | override a camera, e.g. `--camera front=1920x1080@30:4000 --camera top=256x256@5`. Width must be a multiple of 64, and every rate must divide the fastest one |
| `--no-mosaic` | don't offer the mosaic, so cameras render only for their own subscribers |
| `--mosaic-tile 640x360`, `--mosaic-kbps 5000` | mosaic layout and bitrate (3 tiles per row) |
| `--no-shadows` | cheaper rendering (every camera renders its own shadow cascades) |
| `--always-encode` | render and encode everything even without subscribers |
| `--host`, `--control-port`, `--base-port`, `--encoder`, `--gop-seconds` | as for the other producers |

Both demos default to ports 5000–5007. To run sim-streamer and bevy-streamer at the same time, give one of
them other ports (`--control-port 6000 --base-port 6001`) and point receivers at them with `--control-port 6000`.

## sim-streamer vs. bevy-streamer

| | sim-streamer (raw wgpu) | bevy-streamer (Bevy 0.18) |
|---|---|---|
| scene | flat-shaded instanced boxes, fake fog | PBR, sun with shadow cascades, fog, MSAA |
| images | one atlas, one readback for all cameras | one image and one readback per camera |
| per-camera resolution and rate | no (same tile size and rate) | yes |
| unwatched cameras | still rendered (only encoding stops) | not rendered |
| mosaic | the atlas itself | GStreamer `compositor` |
| GStreamer side | `gst_demo::atlas` | `gst_demo::streams` |
| code size | ~600 lines incl. renderer | ~500 lines (scene + glue) |
| build time | seconds | minutes the first time |

## Files

| file | what |
|---|---|
| [src/main.rs](src/main.rs) | headless app, per-camera images and `Readback` observers, `schedule_cameras` (demand + rate), reporting, exit handling |
| [src/scene.rs](src/scene.rs) | world (road, city, park, traffic, drones, lighting), `Lane` driving system, the six `CameraDef`s with their default streams |
| [../src/streams.rs](../src/streams.rs) | shared GStreamer side: an `appsrc` per camera → H.265 channels, compositor mosaic, `demand()` |

## Going further

- **Zero-copy:** instead of `Readback`, a render-graph node can hand the camera's `GpuImage` texture to
  NVENC through D3D12/Vulkan interop.
- **Depth or segmentation** for ML: add `DepthPrepass` or custom passes and read them back as raw buffers
  (not video). See the [sim-streamer README](../sim-streamer/README.md#is-this-the-right-approach).
- **Pause the simulation** when nobody watches anything at all, if the world doesn't need to keep running.
- **Dynamic cameras:** cameras can be added at runtime. The GStreamer side would need a new `appsrc` branch
  (pipelines can be extended while playing) and a control-protocol update.
