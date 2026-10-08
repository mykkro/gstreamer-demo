"""WebRTC gateway: subscribes to one producer channel and re-publishes it to browsers via WebRTC.

    producer :500x (H.265/MPEG-TS) -> tcpclientsrc -> decode -> webrtcsink
                                                            |-- signalling server  ws://<host>:8443
                                                            '-- web server         http://<host>:8081  (index.html)

webrtcsink encodes separately for every browser and adapts the bitrate to each one's network
(congestion control). By default it offers H.264 and VP8, which every browser plays. H.265 can be offered
with --codecs h265,h264,vp8. Chrome negotiates it, but in testing (Chrome, Oct 2026) decoded almost no
frames from webrtcsink's H.265 stream, so it is opt-in.
That per-viewer encoding is the price of WebRTC. For many viewers of the same H.265 stream,
see ../h265-webcodecs.
"""
import argparse
import sys
import time
from pathlib import Path

# reuse the Python demo's helpers (../../python)
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "python"))
from common import DEFAULT_CONTROL_PORT, Gst, init_gst, request  # noqa: E402

WWW = Path(__file__).resolve().parent / "www"
CODEC_CAPS = {"h265": "video/x-h265", "h264": "video/x-h264", "vp8": "video/x-vp8",
              "vp9": "video/x-vp9", "av1": "video/x-av1"}


def build(args, ch):
    caps = ";".join(CODEC_CAPS[c] for c in args.codecs.split(","))
    desc = (
        f"tcpclientsrc host={args.host} port={ch['port']} ! tsdemux latency=0 ! h265parse ! decodebin ! "
        # force system-memory frames: hardware decoders output D3D12 memory, which webrtcsink's
        # codec discovery can't handle ("no codec present that can handle the stream's type")
        f"videoconvert ! video/x-raw,format=I420 ! queue max-size-buffers=2 leaky=downstream ! "
        f"webrtcsink name=ws run-signalling-server=true signalling-server-port={args.signalling_port} "
        f"run-web-server=true web-server-host-addr=http://0.0.0.0:{args.http_port}/ "
        f"web-server-directory=\"{WWW.as_posix()}\" video-caps=\"{caps}\""
    )
    pipeline = Gst.parse_launch(desc)
    ws = pipeline.get_by_name("ws")
    meta = Gst.Structure.new_empty("meta")
    meta.set_value("name", f"gstreamer-demo {ch['id']}")
    ws.set_property("meta", meta)
    if args.stun:
        ws.set_property("stun-server", args.stun)
    return pipeline


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1", help="producer address (default 127.0.0.1)")
    p.add_argument("--control-port", type=int, default=DEFAULT_CONTROL_PORT)
    p.add_argument("--channel", help="producer channel to re-publish (default: the highest resolution)")
    p.add_argument("--codecs", default="h264,vp8",
                   help="codecs offered to browsers, in preference order (default %(default)s)")
    p.add_argument("--http-port", type=int, default=8081, help="web page port (default 8081)")
    p.add_argument("--signalling-port", type=int, default=8443, help="WebSocket signalling port (default 8443)")
    p.add_argument("--stun", default="", help="STUN server, e.g. stun://stun.l.google.com:19302 "
                                                "(not needed on a LAN; default none)")
    args = p.parse_args()

    init_gst()
    if not Gst.ElementFactory.find("webrtcsink"):
        sys.exit("webrtcsink not available (it is part of gst-plugins-rs, included in gstreamer-bundle)")

    while True:
        try:
            channels = request(args.host, args.control_port, "list")["channels"]
            break
        except OSError as exc:
            print(f"Producer {args.host}:{args.control_port} not reachable ({exc}); retrying in 2 s...")
            time.sleep(2)
    if args.channel:
        ch = next((c for c in channels if c["id"] == args.channel), None)
        if ch is None:
            sys.exit(f"Channel {args.channel!r} not offered. Available: {', '.join(c['id'] for c in channels)}")
    else:
        ch = max(channels, key=lambda c: c["width"] * c["height"])

    while True:  # re-subscribe if the producer goes away
        pipeline = build(args, ch)
        pipeline.set_state(Gst.State.PLAYING)
        print(f"Re-publishing producer channel {ch['id']} via WebRTC (codecs: {args.codecs})")
        print(f"  open  http://localhost:{args.http_port}/   (or http://<this-pc-ip>:{args.http_port}/ on the LAN)")
        bus = pipeline.get_bus()
        try:
            while True:
                msg = bus.timed_pop_filtered(200 * Gst.MSECOND, Gst.MessageType.ERROR | Gst.MessageType.EOS)
                if msg is None:
                    continue
                if msg.type == Gst.MessageType.ERROR:
                    err, dbg = msg.parse_error()
                    print(f"ERROR from {msg.src.get_name()}: {err.message}")
                else:
                    print("Producer stream ended")
                break
        except KeyboardInterrupt:
            pipeline.set_state(Gst.State.NULL)
            return
        pipeline.set_state(Gst.State.NULL)
        print("Reconnecting in 2 s...")
        time.sleep(2)


if __name__ == "__main__":
    main()
