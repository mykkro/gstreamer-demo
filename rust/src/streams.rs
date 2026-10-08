//! Engine-agnostic streaming layer for "one image per camera": every camera has its own `appsrc`
//! with its own resolution and frame rate, plus an optional mosaic built by GStreamer's compositor.
//!
//! ```text
//! appsrc front (1280x720@30) ─ tee ─┬─ [valve] ─ H.265 ─ MPEG-TS ─ TCP     channel "front"
//!                                   └─ [mvalve] scale ─┐
//! appsrc rear  (640x360@15)  ─ tee ─┬─ [valve] ─ …     │                   channel "rear"
//!                                   └─ [mvalve] scale ─┤
//!  …                                                   ├─ compositor ─ [valve] ─ H.265 ─ …   channel "mosaic"
//! ```
//!
//! [`CameraStreams::demand`] tells the engine which cameras have to be rendered at all: a camera is
//! needed if its own channel or the mosaic has subscribers. It also opens/closes the mosaic feeds
//! (`mvalve_*`), so nothing is scaled or composited for a mosaic nobody watches.

use anyhow::{Result, ensure};
use gst::prelude::*;

use crate::{Channel, DEFAULT_CONTROL_PORT, channel_branch, pick_encoder, spawn_control_server, wire_channel};

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
    /// Parse "WxH@fps[:kbps]", e.g. "1280x720@30:2500" (missing kbps -> derived from pixels/s).
    pub fn parse(name: &str, spec: &str) -> Result<Self> {
        let (res, kbps) = spec.split_once(':').map_or((spec, None), |(r, k)| (r, Some(k)));
        let (size, fps) = res.split_once('@').ok_or_else(|| anyhow::anyhow!("expected WxH@fps, got {spec:?}"))?;
        let (w, h) = size.split_once('x').ok_or_else(|| anyhow::anyhow!("expected WxH@fps, got {spec:?}"))?;
        let (width, height, fps): (u32, u32, u32) = (w.parse()?, h.parse()?, fps.parse()?);
        let kbps = match kbps {
            Some(k) => k.parse()?,
            None => (width * height * fps / 12000).max(300),
        };
        Ok(StreamSpec { name: name.into(), width, height, fps, kbps })
    }
}

/// The optional mosaic: every camera scaled into a `tile_w`×`tile_h` tile, `cols` tiles per row.
#[derive(Debug, Clone)]
pub struct MosaicSpec {
    pub tile_w: u32,
    pub tile_h: u32,
    pub cols: u32,
    pub fps: u32,
    pub kbps: u32,
}

pub struct CameraStreams {
    pub pipeline: gst::Bin,
    /// one per camera, in the order of the specs
    pub appsrcs: Vec<gst_app::AppSrc>,
    pub channels: Vec<Channel>,
    camera_sinks: Vec<gst::Element>,
    mosaic_sink: Option<gst::Element>,
    mosaic_valves: Vec<gst::Element>,
    always: bool,
}

impl CameraStreams {
    /// Build the pipeline, wire encode-on-demand, start the control server and print the channel table.
    pub fn new(net: &NetArgs, cams: &[StreamSpec], mosaic: Option<&MosaicSpec>, source: &str) -> Result<Self> {
        for c in cams {
            // GPU texture copies use 256-byte rows: keep RGBA rows unpadded
            ensure!((c.width * 4) % 256 == 0, "camera {}: width {} must be a multiple of 64", c.name, c.width);
        }
        let encoder = pick_encoder(&net.encoder)?;
        let mut port = net.base_port;
        let mut mk = |id: &str, width: u32, height: u32, fps: u32, kbps: u32| {
            port += 1;
            Channel {
                id: id.into(),
                width,
                height,
                fps,
                bitrate_kbps: kbps,
                port: port - 1,
                codec: "h265".into(),
                container: "mpegts".into(),
                transport: "tcp".into(),
                encoder: encoder.clone(),
            }
        };
        let mosaic_ch = mosaic.map(|m| {
            let rows = (cams.len() as u32).div_ceil(m.cols);
            mk("mosaic", m.tile_w * m.cols, m.tile_h * rows, m.fps, m.kbps)
        });
        let cam_chs: Vec<Channel> = cams.iter().map(|c| mk(&c.name, c.width, c.height, c.fps, c.kbps)).collect();

        let mut desc = String::new();
        for (i, (spec, ch)) in cams.iter().zip(&cam_chs).enumerate() {
            let overlay = format!(
                "textoverlay text=\"{} {}x{}@{}\" valignment=top halignment=left font-desc=\"Sans 14\" ! ",
                ch.id, spec.width, spec.height, spec.fps
            );
            desc += &format!(
                "appsrc name=src_{i} format=time is-live=true do-timestamp=true \
                 caps=video/x-raw,format=RGBA,width={},height={},framerate={}/1 ! tee name=t_{i} allow-not-linked=true \
                 t_{i}. ! queue max-size-buffers=2 leaky=downstream ! {} ",
                spec.width,
                spec.height,
                spec.fps,
                channel_branch(ch, &overlay, &net.host)
            );
            if let Some(m) = mosaic {
                desc += &format!(
                    "t_{i}. ! queue max-size-buffers=2 leaky=downstream ! valve name=mvalve_{i} drop=true ! \
                     videoscale ! video/x-raw,width={},height={},pixel-aspect-ratio=1/1 ! \
                     textoverlay text=\"{}\" valignment=top halignment=left font-desc=\"Sans 12\" ! comp.sink_{i} ",
                    m.tile_w, m.tile_h, ch.id
                );
            }
        }
        if let (Some(m), Some(ch)) = (mosaic, &mosaic_ch) {
            // live compositor: produces frames at its own rate from whatever each camera delivered last;
            // cameras that are not delivering (inactive) are simply skipped
            let pads: String = (0..cams.len())
                .map(|i| {
                    let x = (i as u32 % m.cols) * m.tile_w;
                    let y = (i as u32 / m.cols) * m.tile_h;
                    format!("sink_{i}::xpos={x} sink_{i}::ypos={y} ")
                })
                .collect();
            desc += &format!(
                "compositor name=comp force-live=true ignore-inactive-pads=true background=black {pads} ! \
                 video/x-raw,width={},height={},framerate={}/1 ! queue max-size-buffers=2 leaky=downstream ! {}",
                ch.width,
                ch.height,
                m.fps,
                channel_branch(ch, "", &net.host)
            );
        }

        let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Bin>().unwrap();
        let mut channels: Vec<Channel> = mosaic_ch.into_iter().collect();
        channels.extend(cam_chs);
        for ch in &channels {
            wire_channel(&pipeline, ch, net.gop_seconds, net.always_encode);
        }
        let get = |n: String| pipeline.by_name(&n).unwrap_or_else(|| panic!("element {n} missing"));
        let appsrcs: Vec<gst_app::AppSrc> = (0..cams.len())
            .map(|i| {
                let src = get(format!("src_{i}")).downcast::<gst_app::AppSrc>().unwrap();
                // never block the engine: keep at most 2 frames queued, drop the oldest
                src.set_property_from_str("max-buffers", "2");
                src.set_property_from_str("leaky-type", "downstream");
                src
            })
            .collect();
        let camera_sinks = cams.iter().map(|c| get(format!("sink_{}", c.name))).collect();
        let mosaic_sink = mosaic.map(|_| get("sink_mosaic".into()));
        let mosaic_valves = if mosaic.is_some() { (0..cams.len()).map(|i| get(format!("mvalve_{i}"))).collect() } else { vec![] };

        let sinks = channels.iter().map(|c| (c.id.clone(), get(format!("sink_{}", c.id)))).collect();
        spawn_control_server(&net.host, net.control_port, source, &channels, sinks)?;

        println!("Source : {source}");
        println!("Encoder: {encoder}");
        println!("Control: tcp://{}:{}", net.host, net.control_port);
        for ch in &channels {
            println!(
                "  {:>7}  {:>4}x{:<4} @ {:>2} fps  {:>5} kbps  -> tcp port {}",
                ch.id, ch.width, ch.height, ch.fps, ch.bitrate_kbps, ch.port
            );
        }
        Ok(CameraStreams { pipeline, appsrcs, channels, camera_sinks, mosaic_sink, mosaic_valves, always: net.always_encode })
    }

    /// Which cameras have to be rendered right now (own channel or mosaic watched).
    /// Call once per engine frame; it also opens/closes the mosaic feeds.
    pub fn demand(&self) -> Vec<bool> {
        let watched = |s: &gst::Element| s.property::<u32>("num-handles") > 0;
        let mosaic = self.always || self.mosaic_sink.as_ref().is_some_and(watched);
        for v in &self.mosaic_valves {
            if v.property::<bool>("drop") == mosaic {
                v.set_property("drop", !mosaic);
            }
        }
        self.camera_sinks.iter().map(|s| self.always || mosaic || watched(s)).collect()
    }

    /// Hand one tightly packed RGBA frame of camera `i` to GStreamer. Returns false once the pipeline shuts down.
    pub fn push_frame(appsrc: &gst_app::AppSrc, rgba: Vec<u8>) -> bool {
        appsrc.push_buffer(gst::Buffer::from_mut_slice(rgba)).is_ok()
    }
}
