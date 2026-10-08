"""Video receiver: asks the producer which channels exist, subscribes to one and displays it.

    control  :5000  {"cmd":"list"}  ->  channel list (resolution, fps, bitrate, port)
    video    :500x  tcpclientsrc -> tsdemux -> h265parse -> decoder -> appsink -> OpenCV window

Keys in the OpenCV window: 1..9 switch channel, q / Esc quit.
"""
import argparse
import json
import socket
import sys
import threading
import time

import numpy as np

from common import DEFAULT_CONTROL_PORT, Gst, GstVideo, has_element, init_gst, request

DECODERS = {
    "auto": "decodebin",         # picks the highest-ranked decoder (usually D3D12/D3D11 hardware)
    "sw": "avdec_h265",          # FFmpeg/libav software decoder
    "d3d11": "d3d11h265dec",
    "d3d12": "d3d12h265dec",
    "nvdec": "nvh265dec",
}


def print_channels(info):
    print(f"Producer source: {info.get('source')}")
    for i, ch in enumerate(info["channels"], 1):
        print(f"  [{i}] {ch['id']:>8}  {ch['width']}x{ch['height']} @ {ch['fps']} fps  "
              f"{ch['codec']}/{ch['container']}  {ch['bitrate_kbps']} kbps  ({ch['encoder']}, port {ch['port']})")


def watch_channels(host, port, channels):
    """Keep `channels` up to date from the producer's pushed events (bevy-streamer adds/removes
    cameras at runtime). Producers without dynamic channels don't push anything."""
    try:
        with socket.create_connection((host, port)) as s:
            s.sendall(b'{"cmd": "watch"}\n')
            f = s.makefile("r", encoding="utf-8")
            if not json.loads(f.readline() or "{}").get("watching"):
                return
            for line in f:
                ev = json.loads(line)
                if ev.get("event") == "channel_added":
                    ch = ev["channel"]
                    channels.append(ch)
                    key = f" - key {len(channels)}" if len(channels) <= 9 else ""
                    print(f"[producer] + channel {ch['id']} ({ch['width']}x{ch['height']} @ {ch['fps']} fps){key}")
                elif ev.get("event") == "channel_removed":
                    channels[:] = [c for c in channels if c["id"] != ev["id"]]
                    print(f"[producer] - channel {ev['id']}")
    except (OSError, ValueError):
        pass


def choose_channel(channels, wanted):
    if wanted:
        for ch in channels:
            if ch["id"] == wanted:
                return channels.index(ch)
        sys.exit(f"Channel {wanted!r} not offered. Available: {', '.join(c['id'] for c in channels)}")
    if sys.stdin.isatty():
        ans = input(f"Select channel [1-{len(channels)}, default 1]: ").strip()
        if ans.isdigit() and 1 <= int(ans) <= len(channels):
            return int(ans) - 1
    return 0


class Receiver:
    def __init__(self, args):
        self.args = args
        self.pipeline = None
        self.appsink = None

    def build(self, ch):
        dec = DECODERS.get(self.args.decoder, self.args.decoder)
        if dec != "decodebin" and not has_element(dec):
            sys.exit(f"Decoder {dec} not available")
        head = (f"tcpclientsrc host={self.args.host} port={ch['port']} ! "
                f"tsdemux latency=0 ! h265parse ! {dec} ! ")
        if self.args.display == "gst":
            tail = "videoconvert ! autovideosink sync=false"
        else:
            tail = ("videoconvert ! video/x-raw,format=BGR ! "
                    "appsink name=sink max-buffers=1 drop=true sync=false")
        self.pipeline = Gst.parse_launch(head + tail)
        self.appsink = self.pipeline.get_by_name("sink")
        self.pipeline.set_state(Gst.State.PLAYING)

    def stop(self):
        if self.pipeline:
            self.pipeline.set_state(Gst.State.NULL)
            self.pipeline = None

    def poll_bus(self):
        """Return an error string if the stream died, else None."""
        bus = self.pipeline.get_bus()
        while True:
            msg = bus.pop_filtered(Gst.MessageType.ERROR | Gst.MessageType.EOS)
            if msg is None:
                return None
            if msg.type == Gst.MessageType.EOS:
                return "end of stream"
            err, _ = msg.parse_error()
            return err.message

    def pull_frame(self, timeout_ms=100):
        sample = self.appsink.emit("try-pull-sample", timeout_ms * Gst.MSECOND)
        if sample is None:
            return None
        vinfo = GstVideo.VideoInfo.new_from_caps(sample.get_caps())
        buf = sample.get_buffer()
        ok, mapinfo = buf.map(Gst.MapFlags.READ)
        if not ok:
            return None
        try:
            w, h, stride = vinfo.width, vinfo.height, vinfo.stride[0]
            # rows may be padded to a multiple of 4 bytes -> honour the stride
            frame = np.frombuffer(mapinfo.data, np.uint8, count=stride * h).reshape(h, stride)
            return frame[:, : w * 3].reshape(h, w, 3).copy()
        finally:
            buf.unmap(mapinfo)


def run(args):
    init_gst()
    rx = Receiver(args)
    try:
        info = request(args.host, args.control_port, "list")
    except OSError as exc:
        sys.exit(f"Cannot reach producer control port {args.host}:{args.control_port}: {exc}")
    print_channels(info)
    if args.list:
        return
    channels = info["channels"]
    idx = choose_channel(channels, args.channel)
    # channels can appear/disappear at runtime: keep the list (and keys 1-9) current
    threading.Thread(target=watch_channels, args=(args.host, args.control_port, channels), daemon=True).start()

    if args.display == "opencv" and not args.no_window:
        import cv2
    win = "gstreamer-demo receiver"
    deadline = time.time() + args.duration if args.duration else None
    total_frames = 0

    while True:  # (re)connect loop
        idx = max(0, min(idx, len(channels) - 1))
        ch = channels[idx]
        print(f"Subscribing to {ch['id']} on {args.host}:{ch['port']}")
        rx.build(ch)
        frames, t0, fps = 0, time.time(), 0.0
        switch_to, error = None, None

        while switch_to is None:
            if deadline and time.time() > deadline:
                print(f"Done: {total_frames} frames received")
                rx.stop()
                return
            error = rx.poll_bus()
            if error:
                break
            if args.display == "gst":
                time.sleep(0.1)
                continue

            frame = rx.pull_frame()
            if frame is not None:
                frames += 1
                total_frames += 1
                now = time.time()
                if now - t0 >= 1.0:
                    fps, frames, t0 = frames / (now - t0), 0, now
                    if args.no_window:
                        print(f"  {ch['id']}: {frame.shape[1]}x{frame.shape[0]}  {fps:.1f} fps")
                if not args.no_window:
                    label = f"{ch['id']}  {frame.shape[1]}x{frame.shape[0]}  {fps:.1f} fps  [1-{len(channels)}] switch, q quit"
                    cv2.putText(frame, label, (10, 30), cv2.FONT_HERSHEY_SIMPLEX, 0.7, (0, 0, 0), 4)
                    cv2.putText(frame, label, (10, 30), cv2.FONT_HERSHEY_SIMPLEX, 0.7, (0, 255, 255), 2)
                    cv2.imshow(win, frame)

            if not args.no_window:
                key = cv2.waitKey(1) & 0xFF
                if key in (ord("q"), 27):
                    rx.stop()
                    return
                if ord("1") <= key <= ord("9") and key - ord("1") < len(channels) and key - ord("1") != idx:
                    switch_to = key - ord("1")
                if frames and cv2.getWindowProperty(win, cv2.WND_PROP_VISIBLE) < 1:
                    rx.stop()
                    return

        rx.stop()
        if switch_to is not None:
            idx = switch_to
            continue
        print(f"Stream lost ({error}); reconnecting in 2 s...")
        time.sleep(2)
        try:  # the producer may have restarted, or this channel was removed
            channels[:] = request(args.host, args.control_port, "list")["channels"]
            ids = [c["id"] for c in channels]
            if ch["id"] in ids:
                idx = ids.index(ch["id"])
            else:
                print(f"Channel {ch['id']} no longer exists, switching to {ids[0]}")
                idx = 0
        except (OSError, IndexError):
            pass


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1", help="producer address (default 127.0.0.1)")
    p.add_argument("--control-port", type=int, default=DEFAULT_CONTROL_PORT)
    p.add_argument("--channel", help="channel id to subscribe to, e.g. 720p30 (default: ask)")
    p.add_argument("--list", action="store_true", help="only print the producer's channels and exit")
    p.add_argument("--decoder", default="auto", help="auto | sw | d3d11 | d3d12 | nvdec | <gst element> (default auto)")
    p.add_argument("--display", choices=["opencv", "gst"], default="opencv",
                   help="opencv: frames go to Python/OpenCV (default); gst: native GStreamer window")
    p.add_argument("--no-window", action="store_true", help="decode only, print fps (benchmark / headless)")
    p.add_argument("--duration", type=float, default=0, help="stop after N seconds (0 = run forever)")
    try:
        run(p.parse_args())
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
