//! Engine-agnostic streaming layer for "one image per camera", with cameras added and removed at
//! runtime. Every camera has its own `appsrc` (own resolution and frame rate) in its own bin, which is
//! added to the running pipeline; an optional mosaic is a fixed grid filled by GStreamer's compositor.
//!
//! ```text
//!  bin "front" (added at runtime)                                     channel "front"   port from the pool
//!  appsrc (1280x720@30) ─ tee ─┬─ [valve] ─ H.265 ─ MPEG-TS ─ TCP
//!                              └─ [mvalve] scale ─ ghost src ─┐
//!  bin "dronecam_1" …                                         │
//!                                                             ├─ compositor (fixed 3×3 grid) ─ [valve] ─ H.265 ─ TCP
//!  videotestsrc black (background, keeps the grid running) ───┘                       channel "mosaic"
//! ```
//!
//! [`CameraStreams::demand`] tells the engine which cameras have to be rendered at all: a camera is
//! needed if its own channel or the mosaic has subscribers. It also opens/closes the mosaic feeds, so
//! nothing is scaled or composited for a mosaic nobody watches.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use gst::prelude::*;

use crate::{Channel, Control, DEFAULT_CONTROL_PORT, channel_branch, pick_encoder, spawn_control_server, wire_channel};

/// Network/encoder options shared by the streamers (use with `#[command(flatten)]`).
#[derive(clap::Args, Debug, Clone)]
pub struct NetArgs {
    /// Interface to listen on
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,
    #[arg(long, default_value_t = DEFAULT_CONTROL_PORT)]
    pub control_port: u16,
    /// First channel port; further channels use consecutive ports
    #[arg(long, default_value_t = 5001)]
    pub base_port: u16,
    /// auto | x265 | nvenc | qsv | amf | mf | <gst element name>
    #[arg(long, default_value = "auto")]
    pub encoder: String,
    #[arg(long, default_value_t = 1)]
    pub gop_seconds: u32,
    /// Encode (and render) everything even with no subscribers
    #[arg(long)]
    pub always_encode: bool,
}

/// One camera stream: name, resolution, frame rate, bitrate.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

impl StreamSpec {
    pub fn new(name: &str, width: u32, height: u32, fps: u32, kbps: Option<u32>) -> Self {
        let kbps = kbps.unwrap_or_else(|| (width * height * fps / 8000).max(500));
        StreamSpec { name: name.into(), width, height, fps, kbps }
    }

    /// Parse "WxH@fps[:kbps]", e.g. "1280x720@30:2500" (missing kbps -> derived from pixels/s).
    pub fn parse(name: &str, spec: &str) -> Result<Self> {
        let (res, kbps) = spec.split_once(':').map_or((spec, None), |(r, k)| (r, Some(k)));
        let (size, fps) = res.split_once('@').with_context(|| format!("expected WxH@fps, got {spec:?}"))?;
        let (w, h) = size.split_once('x').with_context(|| format!("expected WxH@fps, got {spec:?}"))?;
        Ok(Self::new(name, w.parse()?, h.parse()?, fps.parse()?, kbps.map(str::parse).transpose()?))
    }
}

/// The optional mosaic: a fixed grid of `cols`×`rows` tiles; cameras take free slots.
#[derive(Debug, Clone)]
pub struct MosaicSpec {
    pub tile_w: u32,
    pub tile_h: u32,
    pub cols: u32,
    pub rows: u32,
    pub fps: u32,
    pub kbps: u32,
}

struct Mosaic {
    spec: MosaicSpec,
    compositor: gst::Element,
    sink: gst::Element,
    background_valve: gst::Element,
}

struct Camera {
    spec: StreamSpec,
    channel: Channel,
    bin: gst::Bin,
    sink: gst::Element,
    /// mosaic feed: valve in the camera's bin, compositor pad, grid slot
    mosaic: Option<(gst::Element, gst::Pad, usize)>,
}

pub struct CameraStreams {
    pub pipeline: gst::Bin,
    pub control: Arc<Control>,
    net: NetArgs,
    encoder: String,
    max_cameras: usize,
    mosaic: Option<Mosaic>,
    cameras: Vec<Camera>,
}

/// Channel ids end up in GStreamer element names and pipeline descriptions: keep them simple.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 24 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl CameraStreams {
    /// Create the pipeline (with only the mosaic, if any) and start the control server.
    /// Cameras are added with [`add_camera`](Self::add_camera); the pipeline is started by
    /// [`crate::run_main_loop`].
    pub fn new(net: &NetArgs, mosaic: Option<MosaicSpec>, max_cameras: usize, source: &str) -> Result<Self> {
        let encoder = pick_encoder(&net.encoder)?;
        let mut channels = vec![];
        let (pipeline, mosaic) = match mosaic {
            None => (gst::Pipeline::new().upcast::<gst::Bin>(), None),
            Some(m) => {
                let ch = Channel {
                    id: "mosaic".into(),
                    width: m.tile_w * m.cols,
                    height: m.tile_h * m.rows,
                    fps: m.fps,
                    bitrate_kbps: m.kbps,
                    port: net.base_port,
                    codec: "h265".into(),
                    container: "mpegts".into(),
                    transport: "tcp".into(),
                    encoder: encoder.clone(),
                };
                // A black background source keeps the grid running at a fixed size and rate even when
                // cameras come and go; its valve is closed while nobody watches the mosaic.
                // The background drives the output clock in real time, so the compositor needs some
                // latency budget: camera frames arrive after readback + scaling and would otherwise be
                // "late" and dropped (black tiles).
                let latency = 100_000_000u64; // ns
                let desc = format!(
                    "videotestsrc is-live=true pattern=black ! video/x-raw,width={w},height={h},framerate={fps}/1 ! \
                     valve name=mvalve_background drop=true ! \
                     compositor name=comp force-live=true ignore-inactive-pads=true background=black latency={latency} ! \
                     video/x-raw,width={w},height={h},framerate={fps}/1 ! queue max-size-buffers=2 leaky=downstream ! {}",
                    channel_branch(&ch, "", &net.host),
                    w = ch.width,
                    h = ch.height,
                    fps = m.fps,
                );
                // top level of the pipeline (not a sub-bin): camera bins link straight to the compositor
                let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Bin>().unwrap();
                wire_channel(&pipeline, &ch, net.gop_seconds, net.always_encode);
                let get = |n: &str| pipeline.by_name(n).unwrap();
                let mosaic = Mosaic {
                    compositor: get("comp"),
                    sink: get("sink_mosaic"),
                    background_valve: get("mvalve_background"),
                    spec: m,
                };
                channels.push((ch, mosaic.sink.clone()));
                (pipeline, Some(mosaic))
            }
        };
        let (chs, sinks): (Vec<Channel>, Vec<(String, gst::Element)>) =
            channels.into_iter().map(|(c, s)| (c.clone(), (c.id, s))).unzip();
        let control = spawn_control_server(&net.host, net.control_port, source, &chs, sinks)?;

        println!("Source : {source}");
        println!("Encoder: {encoder}");
        println!("Control: tcp://{}:{}", net.host, net.control_port);
        for c in &chs {
            println!("  {:>10}  {:>4}x{:<4} @ {:>2} fps  {:>5} kbps  -> tcp port {}", c.id, c.width, c.height, c.fps, c.bitrate_kbps, c.port);
        }
        Ok(CameraStreams { pipeline, control, net: net.clone(), encoder, max_cameras, mosaic, cameras: vec![] })
    }

    pub fn camera_names(&self) -> Vec<String> {
        self.cameras.iter().map(|c| c.spec.name.clone()).collect()
    }

    /// Add a camera stream to the (possibly running) pipeline: allocates a port and a mosaic slot,
    /// publishes the channel to the control server (watchers get `channel_added`).
    pub fn add_camera(&mut self, spec: &StreamSpec) -> Result<(Channel, gst_app::AppSrc)> {
        let name = &spec.name;
        ensure!(valid_name(name) && name != "mosaic", "camera name {name:?}: use 1-24 chars of a-z, 0-9, _");
        ensure!(self.cameras.iter().all(|c| &c.spec.name != name), "camera {name:?} already exists");
        ensure!(self.cameras.len() < self.max_cameras, "camera limit reached ({})", self.max_cameras);
        // GPU texture copies use 256-byte rows: keep RGBA rows unpadded
        ensure!((spec.width * 4) % 256 == 0, "camera {name}: width {} must be a multiple of 64", spec.width);
        ensure!((16..=4096).contains(&spec.height) && (1..=120).contains(&spec.fps), "camera {name}: bad size or rate");

        // lowest free port after the mosaic's
        let port = (1..=self.max_cameras as u16)
            .map(|i| self.net.base_port + i)
            .find(|p| self.cameras.iter().all(|c| c.channel.port != *p))
            .context("no free port")?;
        let channel = Channel {
            id: name.clone(),
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            bitrate_kbps: spec.kbps,
            port,
            codec: "h265".into(),
            container: "mpegts".into(),
            transport: "tcp".into(),
            encoder: self.encoder.clone(),
        };
        // lowest free mosaic slot (a camera without a slot still has its own channel)
        let slot = self.mosaic.as_ref().and_then(|m| {
            (0..(m.spec.cols * m.spec.rows) as usize).find(|s| self.cameras.iter().all(|c| c.mosaic.as_ref().map(|x| x.2) != Some(*s)))
        });

        let overlay = format!(
            "textoverlay text=\"{name} {}x{}@{}\" valignment=top halignment=left font-desc=\"Sans 14\" ! ",
            spec.width, spec.height, spec.fps
        );
        let mut desc = format!(
            "appsrc name=src_{name} format=time is-live=true do-timestamp=true \
             caps=video/x-raw,format=RGBA,width={},height={},framerate={}/1 ! tee name=t_{name} allow-not-linked=true \
             t_{name}. ! queue max-size-buffers=2 leaky=downstream ! {} ",
            spec.width,
            spec.height,
            spec.fps,
            channel_branch(&channel, &overlay, &self.net.host)
        );
        if let (Some(m), Some(_)) = (&self.mosaic, slot) {
            // the end of this branch (queue "mout_<name>") gets a ghost "src" pad -> compositor
            let open = m.sink.property::<u32>("num-handles") > 0 || self.net.always_encode;
            desc += &format!(
                "t_{name}. ! queue max-size-buffers=2 leaky=downstream ! valve name=mvalve_{name} drop={} ! \
                 videoscale ! video/x-raw,width={},height={},pixel-aspect-ratio=1/1 ! \
                 textoverlay text=\"{name}\" valignment=top halignment=left font-desc=\"Sans 12\" ! \
                 queue name=mout_{name} max-size-buffers=2",
                !open, m.spec.tile_w, m.spec.tile_h
            );
        }
        // No automatic ghosting of unlinked pads: that would also ghost textoverlay's "text_sink",
        // which then counts as linked and makes textoverlay wait forever for text (frozen video).
        let bin = gst::parse::bin_from_description(&desc, false)?;
        if let Some(out) = bin.by_name(&format!("mout_{name}")) {
            let ghost = gst::GhostPad::with_target(&out.static_pad("src").unwrap())?;
            ghost.set_property("name", "src");
            bin.add_pad(&ghost)?;
        }
        bin.set_property("name", format!("cam_{name}"));
        self.pipeline.add(&bin)?;
        match self.attach(&bin, spec, &channel, slot) {
            Ok(r) => Ok(r),
            Err(e) => {
                // roll back: nothing of a failed camera stays in the pipeline
                let _ = bin.set_state(gst::State::Null);
                let _ = self.pipeline.remove(&bin);
                Err(e)
            }
        }
    }

    fn attach(&mut self, bin: &gst::Bin, spec: &StreamSpec, channel: &Channel, slot: Option<usize>) -> Result<(Channel, gst_app::AppSrc)> {
        let (name, port) = (&spec.name, channel.port);
        wire_channel(&self.pipeline, channel, self.net.gop_seconds, self.net.always_encode);
        let get = |n: String| bin.by_name(&n).with_context(|| format!("element {n} missing"));
        let appsrc = get(format!("src_{name}"))?.downcast::<gst_app::AppSrc>().unwrap();
        // never block the engine: keep at most 2 frames queued, drop the oldest
        appsrc.set_property_from_str("max-buffers", "2");
        appsrc.set_property_from_str("leaky-type", "downstream");
        let sink = get(format!("sink_{name}"))?;

        let mosaic = match (&self.mosaic, slot) {
            (Some(m), Some(slot)) => {
                let pad = m.compositor.request_pad_simple("sink_%u").context("compositor pad")?;
                pad.set_property("xpos", ((slot as u32 % m.spec.cols) * m.spec.tile_w) as i32);
                pad.set_property("ypos", ((slot as u32 / m.spec.cols) * m.spec.tile_h) as i32);
                pad.set_property("zorder", 1u32); // above the black background
                if let Err(e) = bin.static_pad("src").context("mosaic ghost pad").and_then(|src| Ok(src.link(&pad)?)) {
                    m.compositor.release_request_pad(&pad);
                    return Err(e);
                }
                Some((get(format!("mvalve_{name}"))?, pad, slot))
            }
            _ => None,
        };
        bin.sync_state_with_parent()?;
        self.control.add_channel(channel.clone(), sink.clone());
        println!(
            "+ camera {name:<10} {:>4}x{:<4} @ {:>2} fps {:>5} kbps -> tcp port {port}{}",
            spec.width,
            spec.height,
            spec.fps,
            spec.kbps,
            mosaic.as_ref().map(|m| format!(", mosaic slot {}", m.2)).unwrap_or_default()
        );
        self.cameras.push(Camera { spec: spec.clone(), channel: channel.clone(), bin: bin.clone(), sink, mosaic });
        Ok((channel.clone(), appsrc))
    }

    /// Remove a camera: withdraw the channel (watchers get `channel_removed`, subscribers are
    /// disconnected), stop and remove its bin, free its port and mosaic slot.
    pub fn remove_camera(&mut self, name: &str) -> Result<()> {
        let Some(i) = self.cameras.iter().position(|c| c.spec.name == name) else {
            bail!("no camera {name:?}");
        };
        let cam = self.cameras.remove(i);
        self.control.remove_channel(name);
        // A source-only branch can simply be stopped: nothing upstream is waiting for it.
        cam.bin.set_locked_state(true);
        cam.bin.set_state(gst::State::Null)?;
        if let (Some(m), Some((_, pad, _))) = (&self.mosaic, &cam.mosaic) {
            if let Some(src) = cam.bin.static_pad("src") {
                let _ = src.unlink(pad);
            }
            m.compositor.release_request_pad(pad);
        }
        self.pipeline.remove(&cam.bin)?;
        println!("- camera {name}");
        Ok(())
    }

    /// Which cameras have to be rendered right now (own channel or mosaic watched), by name.
    /// Call once per engine frame; it also opens/closes the mosaic feeds.
    pub fn demand(&self) -> HashMap<String, bool> {
        let watched = |s: &gst::Element| s.property::<u32>("num-handles") > 0;
        let always = self.net.always_encode;
        let mosaic = always || self.mosaic.as_ref().is_some_and(|m| watched(&m.sink));
        let set_open = |v: &gst::Element| {
            if v.property::<bool>("drop") == mosaic {
                v.set_property("drop", !mosaic);
            }
        };
        if let Some(m) = &self.mosaic {
            set_open(&m.background_valve);
        }
        self.cameras
            .iter()
            .map(|c| {
                if let Some((v, _, _)) = &c.mosaic {
                    set_open(v);
                }
                let in_mosaic = mosaic && c.mosaic.is_some();
                (c.spec.name.clone(), always || in_mosaic || watched(&c.sink))
            })
            .collect()
    }

    /// Hand one tightly packed RGBA frame to a camera's appsrc. Returns false if it is gone/shutting down.
    pub fn push_frame(appsrc: &gst_app::AppSrc, rgba: Vec<u8>) -> bool {
        appsrc.push_buffer(gst::Buffer::from_mut_slice(rgba)).is_ok()
    }
}
