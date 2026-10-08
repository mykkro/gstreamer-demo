//! Engine-agnostic streaming layer for "several cameras rendered into one atlas image".
//!
//! The renderer (raw wgpu in `sim-streamer`, Bevy in `bevy-streamer`, …) only has to push one
//! RGBA atlas per frame into [`AtlasStream::push_frame`]. Everything after that is GStreamer:
//!
//! ```text
//! appsrc (RGBA atlas) ─ tee ─┬─ [valve] ─ H.265 ─ MPEG-TS ─ TCP   channel "mosaic"
//!                            ├─ [valve] ─ crop tile 0 ─ H.265 ─ … channel <camera 0>
//!                            └─ …
//! + control server (list / stats), same protocol as the producer
//! ```

use anyhow::Result;
use gst::prelude::*;

use crate::streams::NetArgs;
use crate::{Channel, channel_branch, pick_encoder, spawn_control_server, wire_channel};

/// Command-line options shared by the atlas streamers (use with `#[command(flatten)]`).
#[derive(clap::Args, Debug, Clone)]
pub struct AtlasArgs {
    #[command(flatten)]
    pub net: NetArgs,
    /// Width of one camera image (multiple of 64: GPU copies need 256-byte rows)
    #[arg(long, default_value_t = 640)]
    pub cam_width: u32,
    /// Height of one camera image
    #[arg(long, default_value_t = 360)]
    pub cam_height: u32,
    /// Cameras per atlas row
    #[arg(long, default_value_t = 3)]
    pub cols: u32,
    #[arg(long, default_value_t = 30)]
    pub fps: u32,
    /// Bitrate per camera channel (kbps)
    #[arg(long, default_value_t = 1200)]
    pub cam_kbps: u32,
    /// Bitrate of the mosaic channel (kbps)
    #[arg(long, default_value_t = 5000)]
    pub mosaic_kbps: u32,
}

impl AtlasArgs {
    pub fn rows(&self, cameras: usize) -> u32 {
        (cameras as u32).div_ceil(self.cols)
    }

    pub fn atlas_size(&self, cameras: usize) -> (u32, u32) {
        (self.cam_width * self.cols, self.cam_height * self.rows(cameras))
    }

    /// Top-left pixel of camera `i` in the atlas.
    pub fn tile_origin(&self, i: usize) -> (u32, u32) {
        ((i as u32 % self.cols) * self.cam_width, (i as u32 / self.cols) * self.cam_height)
    }
}

pub struct AtlasStream {
    pub pipeline: gst::Bin,
    pub appsrc: gst_app::AppSrc,
    pub channels: Vec<Channel>,
    pub width: u32,
    pub height: u32,
}

impl AtlasStream {
    /// Build the pipeline, wire encode-on-demand, start the control server and print the channel table.
    /// The pipeline is not started yet; use [`crate::run_main_loop`].
    pub fn new(args: &AtlasArgs, camera_names: &[&str], source: &str) -> Result<Self> {
        let (width, height) = args.atlas_size(camera_names.len());
        anyhow::ensure!(
            (args.cam_width * 4) % 256 == 0,
            "--cam-width must be a multiple of 64 (GPU texture copies need 256-byte aligned rows)"
        );
        let encoder = pick_encoder(&args.net.encoder)?;
        let mk = |i: usize, id: &str, w: u32, h: u32, kbps: u32| Channel {
            id: id.into(),
            width: w,
            height: h,
            fps: args.fps,
            bitrate_kbps: kbps,
            port: args.net.base_port + i as u16,
            codec: "h265".into(),
            container: "mpegts".into(),
            transport: "tcp".into(),
            encoder: encoder.clone(),
        };
        let mut channels = vec![mk(0, "mosaic", width, height, args.mosaic_kbps)];
        for (i, name) in camera_names.iter().enumerate() {
            channels.push(mk(i + 1, name, args.cam_width, args.cam_height, args.cam_kbps));
        }

        // Per-camera channels crop their tile *after* the valve, so cropping and encoding only
        // happen for channels somebody is watching.
        let mut desc = format!(
            "appsrc name=src format=time is-live=true do-timestamp=true \
             caps=video/x-raw,format=RGBA,width={width},height={height},framerate={}/1 ! \
             tee name=t allow-not-linked=true \
             t. ! queue max-size-buffers=2 leaky=downstream ! {}",
            args.fps,
            channel_branch(&channels[0], "", &args.net.host)
        );
        for (i, ch) in channels.iter().enumerate().skip(1) {
            let (x, y) = args.tile_origin(i - 1);
            let pre = format!(
                "videocrop left={x} top={y} right={} bottom={} ! \
                 textoverlay text=\"{}\" valignment=top halignment=left font-desc=\"Sans 14\" ! ",
                width - x - args.cam_width,
                height - y - args.cam_height,
                ch.id
            );
            desc += &format!(" t. ! queue max-size-buffers=2 leaky=downstream ! {}", channel_branch(ch, &pre, &args.net.host));
        }
        let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Bin>().unwrap();
        for ch in &channels {
            wire_channel(&pipeline, ch, args.net.gop_seconds, args.net.always_encode);
        }
        let appsrc = pipeline.by_name("src").unwrap().downcast::<gst_app::AppSrc>().unwrap();
        // never block the renderer: keep at most 2 frames queued, drop the oldest
        appsrc.set_property_from_str("max-buffers", "2");
        appsrc.set_property_from_str("leaky-type", "downstream");

        let sinks = channels
            .iter()
            .map(|c| (c.id.clone(), pipeline.by_name(&format!("sink_{}", c.id)).unwrap()))
            .collect();
        spawn_control_server(&args.net.host, args.net.control_port, source, &channels, sinks)?;

        println!("Source : {source}");
        println!(
            "Atlas  : {width}x{height} ({} cameras of {}x{}) @ {} fps",
            camera_names.len(),
            args.cam_width,
            args.cam_height,
            args.fps
        );
        println!("Encoder: {encoder}");
        println!("Control: tcp://{}:{}", args.net.host, args.net.control_port);
        for ch in &channels {
            println!("  {:>7}  {}x{}  {} kbps  -> tcp port {}", ch.id, ch.width, ch.height, ch.bitrate_kbps, ch.port);
        }
        Ok(AtlasStream { pipeline, appsrc, channels, width, height })
    }

    /// Hand one tightly packed RGBA atlas to GStreamer. Returns false once the pipeline shuts down.
    pub fn push_frame(appsrc: &gst_app::AppSrc, rgba: Vec<u8>) -> bool {
        appsrc.push_buffer(gst::Buffer::from_mut_slice(rgba)).is_ok()
    }
}
