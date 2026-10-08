"""Video producer: decodes one source once, offers several H.265 channels.

    source (mp4, looped | test pattern)
        -> decode -> tee -+-> [valve] scale/rate -> H.265 enc -> MPEG-TS -> tcpserversink :5001
                          +-> [valve] scale/rate -> H.265 enc -> MPEG-TS -> tcpserversink :5002
                          +-> ...
    control server (JSON over TCP, :5000) answers "what channels do you have?"

Each channel is encoded once no matter how many subscribers are connected
(tcpserversink fans the same bytes out to every client). A channel with no
subscribers is gated off by its valve, so it costs no scaling/encoding CPU.
"""
import argparse
import json
import os
import re
import socketserver
import sys
import threading
from pathlib import Path

from common import DEFAULT_CONTROL_PORT, Gst, GstVideo, has_element, init_gst
from gi.repository import GLib

# Encoder preference for --encoder auto: hardware first, x265 (CPU) as fallback.
AUTO_ENCODERS = ["nvh265enc", "qsvh265enc", "amfh265enc", "x265enc"]
ENCODER_ALIASES = {
    "x265": "x265enc", "nvenc": "nvh265enc", "qsv": "qsvh265enc",
    "amf": "amfh265enc", "mf": "mfh265enc",
}


def parse_channels(spec):
    """'1080p30:4000,720p30:2000' -> list of channel dicts."""
    channels = []
    for item in spec.split(","):
        m = re.fullmatch(r"\s*(\d+)p(\d+)(?::(\d+))?\s*", item)
        if not m:
            raise argparse.ArgumentTypeError(f"bad channel spec {item!r}, expected e.g. 720p30:2000")
        height, fps = int(m[1]), int(m[2])
        width = round(height * 16 / 9 / 2) * 2  # 16:9, even width
        kbps = int(m[3]) if m[3] else max(300, width * height * fps // 15000)
        channels.append(dict(id=f"{height}p{fps}", width=width, height=height, fps=fps, bitrate_kbps=kbps))
    return channels


def pick_encoder(choice):
    if choice == "auto":
        for name in AUTO_ENCODERS:
            if has_element(name):
                return name
        sys.exit("No H.265 encoder found (is gstreamer-bundle installed?)")
    name = ENCODER_ALIASES.get(choice, choice)
    if not has_element(name):
        sys.exit(f"Encoder {name} is not available on this machine")
    return name


def set_props(element, props):
    """Set properties that exist on this element; enums may be given by nick."""
    for key, value in props.items():
        if element.find_property(key) is None:
            continue
        try:
            if isinstance(value, str):
                Gst.util_set_object_arg(element, key, value)
            else:
                element.set_property(key, value)
        except Exception as exc:  # noqa: BLE001 - property differences between versions
            print(f"  (ignoring {element.get_name()}.{key}={value}: {exc})")


def encoder_props(name, ch, gop):
    """Low-latency, CBR-ish settings per encoder family."""
    kbps = ch["bitrate_kbps"]
    if name == "x265enc":
        # ultrafast + zerolatency: no B-frames, no lookahead -> lowest delay, CPU-friendly
        return {"speed-preset": "ultrafast", "tune": "zerolatency", "bitrate": kbps, "key-int-max": gop}
    if name == "nvh265enc":
        return {"preset": "p4", "tune": "low-latency", "rc-mode": "cbr", "bitrate": kbps,
                "gop-size": gop, "zerolatency": True, "repeat-sequence-header": True}
    if name == "mfh265enc":
        return {"rc-mode": "cbr", "bitrate": kbps, "gop-size": gop, "low-latency": True}
    # qsv / amf and others: the common subset
    return {"bitrate": kbps, "gop-size": gop}


class Producer:
    def __init__(self, args):
        self.args = args
        self.channels = parse_channels(args.channels)
        self.encoder = pick_encoder(args.encoder)
        for i, ch in enumerate(self.channels):
            ch["port"] = args.base_port + i
        self.loop = GLib.MainLoop()
        self.pipeline = Gst.parse_launch(self._describe())
        self._configure()

    # ---- pipeline -------------------------------------------------------
    def _source_desc(self):
        if self.args.source == "test":
            return ("videotestsrc is-live=true pattern=smpte ! "
                    "video/x-raw,width=1920,height=1080,framerate=30/1 ! "
                    "timeoverlay font-desc=\"Sans 36\" ! videoconvert name=srcconv")
        uri = Path(self.args.source).resolve().as_uri()
        # clocksync paces decoding to real time, even when no channel is consumed
        return f"uridecodebin uri=\"{uri}\" ! videoconvert name=srcconv ! clocksync"

    def _describe(self):
        parts = [self._source_desc() + " ! tee name=t allow-not-linked=true"]
        overlay_ok = has_element("textoverlay")
        for ch in self.channels:
            cid = ch["id"]
            overlay = (f"textoverlay text=\"{cid} {self.encoder}\" valignment=top halignment=right "
                       f"font-desc=\"Sans 20\" ! " if overlay_ok else "")
            parts.append(
                # videorate sits before the valve so it never "fills" the closed-valve gap with duplicates
                f"t. ! queue max-size-buffers=3 leaky=downstream ! "
                f"videorate skip-to-first=true ! video/x-raw,framerate={ch['fps']}/1 ! "
                f"valve name=valve_{cid} drop=true ! videoscale ! "
                f"video/x-raw,width={ch['width']},height={ch['height']},pixel-aspect-ratio=1/1 ! "
                f"{overlay}videoconvert ! {self.encoder} name=enc_{cid} ! "
                f"h265parse config-interval=-1 ! "            # VPS/SPS/PPS before every IDR -> late joiners can decode
                f"mpegtsmux alignment=7 ! "                    # 7 TS packets = 1316 B chunks
                f"tcpserversink name=sink_{cid} host={self.args.host} port={ch['port']} "
                f"sync=false async=false sync-method=next-keyframe recover-policy=keyframe"
            )
        return " ".join(parts)

    def _configure(self):
        for ch in self.channels:
            cid = ch["id"]
            gop = ch["fps"] * self.args.gop_seconds
            set_props(self.pipeline.get_by_name(f"enc_{cid}"), encoder_props(self.encoder, ch, gop))
            sink = self.pipeline.get_by_name(f"sink_{cid}")
            sink.connect("client-added", self._on_client_added, cid)
            sink.connect("client-socket-removed", self._on_client_removed, cid)
            if self.args.always_encode:
                self.pipeline.get_by_name(f"valve_{cid}").set_property("drop", False)

        bus = self.pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", self._on_bus_message)

        if self.args.source != "test":
            # Seamless looping: once the first frame arrives, do a flushing *segment* seek.
            # At the end the demuxer posts SEGMENT_DONE instead of EOS and we seek again
            # without flushing, so timestamps keep running and clients never see EOS.
            pad = self.pipeline.get_by_name("srcconv").get_static_pad("sink")
            pad.add_probe(Gst.PadProbeType.BUFFER, self._on_first_buffer)

    def _on_first_buffer(self, pad, info):
        GLib.idle_add(self._seek, Gst.SeekFlags.FLUSH | Gst.SeekFlags.SEGMENT)
        return Gst.PadProbeReturn.REMOVE

    def _seek(self, flags):
        conv = self.pipeline.get_by_name("srcconv")
        ok = conv.send_event(Gst.Event.new_seek(1.0, Gst.Format.TIME, flags,
                                                Gst.SeekType.SET, 0, Gst.SeekType.NONE, -1))
        if not ok:
            print("warning: seek failed, file will not loop")
        return False

    # ---- subscribers ----------------------------------------------------
    def _on_client_added(self, sink, _socket, cid):
        print(f"[{cid}] subscriber connected ({sink.get_property('num-handles')} total)")
        self.pipeline.get_by_name(f"valve_{cid}").set_property("drop", False)
        # Ask the encoder for an IDR frame right away so the new client starts fast
        enc_src = self.pipeline.get_by_name(f"enc_{cid}").get_static_pad("src")
        enc_src.send_event(GstVideo.video_event_new_upstream_force_key_unit(Gst.CLOCK_TIME_NONE, True, 0))

    def _on_client_removed(self, sink, _socket, cid):
        GLib.idle_add(self._maybe_close_valve, sink, cid)

    def _maybe_close_valve(self, sink, cid):
        n = sink.get_property("num-handles")
        print(f"[{cid}] subscriber left ({n} remaining)")
        if n == 0 and not self.args.always_encode:
            self.pipeline.get_by_name(f"valve_{cid}").set_property("drop", True)
        return False

    def clients(self):
        return {ch["id"]: self.pipeline.get_by_name(f"sink_{ch['id']}").get_property("num-handles")
                for ch in self.channels}

    # ---- control --------------------------------------------------------
    def info(self):
        return {
            "ok": True,
            "source": self.args.source,
            "channels": [dict(ch, codec="h265", container="mpegts", transport="tcp", encoder=self.encoder)
                         for ch in self.channels],
        }

    def handle(self, msg):
        cmd = msg.get("cmd")
        if cmd in ("list", "info"):
            return self.info()
        if cmd == "stats":
            return {"ok": True, "clients": self.clients()}
        return {"ok": False, "error": f"unknown command {cmd!r}"}

    def _on_bus_message(self, _bus, msg):
        t = msg.type
        if t == Gst.MessageType.SEGMENT_DONE:
            self._seek(Gst.SeekFlags.SEGMENT)
        elif t == Gst.MessageType.EOS:
            print("End of stream")
            self.loop.quit()
        elif t == Gst.MessageType.ERROR:
            err, dbg = msg.parse_error()
            print(f"ERROR from {msg.src.get_name()}: {err.message}\n  {dbg}")
            self.loop.quit()
        elif t == Gst.MessageType.WARNING:
            err, _ = msg.parse_warning()
            print(f"WARNING from {msg.src.get_name()}: {err.message}")

    def run(self):
        producer = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                for line in self.rfile:
                    try:
                        reply = producer.handle(json.loads(line))
                    except Exception as exc:  # noqa: BLE001
                        reply = {"ok": False, "error": str(exc)}
                    self.wfile.write((json.dumps(reply) + "\n").encode())

        socketserver.ThreadingTCPServer.allow_reuse_address = True
        server = socketserver.ThreadingTCPServer((self.args.host, self.args.control_port), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()

        print(f"Source : {self.args.source}")
        print(f"Encoder: {self.encoder}")
        print(f"Control: tcp://{self.args.host}:{self.args.control_port}")
        for ch in self.channels:
            print(f"  {ch['id']:>8}  {ch['width']}x{ch['height']}@{ch['fps']}  "
                  f"{ch['bitrate_kbps']} kbps  -> tcp port {ch['port']}")

        self.pipeline.set_state(Gst.State.PLAYING)
        GLib.timeout_add(250, lambda: True)  # wake the loop so Ctrl+C is noticed on Windows
        try:
            self.loop.run()
        except KeyboardInterrupt:
            pass
        finally:
            print("Stopping...")
            server.shutdown()
            self.pipeline.set_state(Gst.State.NULL)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("source", nargs="?", default="test", help="path to a video file (looped) or 'test' (default)")
    p.add_argument("--host", default="0.0.0.0", help="interface to listen on (default all)")
    p.add_argument("--control-port", type=int, default=DEFAULT_CONTROL_PORT)
    p.add_argument("--base-port", type=int, default=5001, help="first channel port; channels use consecutive ports")
    p.add_argument("--channels", default="1080p30:4000,720p30:2000,480p15:600",
                   help="comma list of <height>p<fps>[:kbps] (default %(default)s)")
    p.add_argument("--encoder", default="auto",
                   help="auto | x265 | nvenc | qsv | amf | mf | <gst element name> (default auto)")
    p.add_argument("--gop-seconds", type=int, default=2, help="keyframe interval in seconds (default 2)")
    p.add_argument("--always-encode", action="store_true", help="encode all channels even with no subscribers")
    args = p.parse_args()

    if args.source != "test" and not os.path.isfile(args.source):
        sys.exit(f"File not found: {args.source}")
    init_gst()
    Producer(args).run()


if __name__ == "__main__":
    main()
