"""Control a producer's cameras at runtime (bevy-streamer), or watch its channel list change.

    python camctl.py list
    python camctl.py watch                                     # print channel_added / channel_removed
    python camctl.py add cctv1 --attach world --position 0 40 110 --look-at ego --size 640x360 --fps 15
    python camctl.py add hood --attach ego --position 0 1.2 -1.5 --look-at forward --size 1280x720 --fps 30
    python camctl.py add follow2 --attach car:2 --position 0 3 6 --look-at 0 1 -10 --lifetime 30
    python camctl.py remove cctv1

Attach targets: world | ego | car:N | drone:N. --look-at: x y z | forward | ego | car:N | drone:N.
Positions are relative to the attach target (world coordinates for "world").
"""
import argparse
import json
import socket
import sys

DEFAULT_CONTROL_PORT = 5000  # same as common.py (kept standalone: no GStreamer needed)


def send(host, port, msg, timeout=8.0):
    with socket.create_connection((host, port), timeout=timeout) as s:
        s.sendall((json.dumps(msg) + "\n").encode())
        with s.makefile("r", encoding="utf-8") as f:
            return json.loads(f.readline())


def print_channels(channels):
    for ch in channels:
        print(f"  {ch['id']:>12}  {ch['width']:>4}x{ch['height']:<4} @ {ch['fps']:>2} fps  {ch['bitrate_kbps']:>5} kbps  port {ch['port']}")


def watch(host, port):
    with socket.create_connection((host, port)) as s:
        s.sendall(b'{"cmd": "watch"}\n')
        f = s.makefile("r", encoding="utf-8")
        first = json.loads(f.readline())
        print(f"{first.get('source')}: {len(first['channels'])} channels" + ("" if first.get("watching") else
              "  (producer does not push changes)"))
        print_channels(first["channels"])
        for line in f:
            ev = json.loads(line)
            if ev.get("event") == "channel_added":
                print("+ added  ", end="")
                print_channels([ev["channel"]])
            elif ev.get("event") == "channel_removed":
                print(f"- removed  {ev['id']}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--control-port", type=int, default=DEFAULT_CONTROL_PORT)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    sub.add_parser("watch")
    a = sub.add_parser("add")
    a.add_argument("name", help="channel name: a-z, 0-9, _ (max 24)")
    a.add_argument("--attach", default="world", help="world | ego | car:N | drone:N (default world)")
    a.add_argument("--position", type=float, nargs=3, metavar=("X", "Y", "Z"))
    a.add_argument("--look-at", nargs="+", metavar="TARGET", help="x y z | forward | ego | car:N | drone:N")
    a.add_argument("--size", default="640x360", help="WxH, width a multiple of 64 (default 640x360)")
    a.add_argument("--fps", type=int, default=15, help="must divide the engine tick rate (default 15)")
    a.add_argument("--kbps", type=int)
    a.add_argument("--fov", type=float, help="vertical field of view in degrees (default 60)")
    a.add_argument("--lifetime", type=float, help="remove automatically after N seconds")
    r = sub.add_parser("remove")
    r.add_argument("name")
    args = p.parse_args()

    try:
        if args.cmd == "list":
            info = send(args.host, args.control_port, {"cmd": "list"})
            print(f"{info.get('source')}:")
            print_channels(info["channels"])
            return
        if args.cmd == "watch":
            watch(args.host, args.control_port)
            return
        if args.cmd == "add":
            w, h = (int(v) for v in args.size.split("x"))
            msg = {"cmd": "add_camera", "name": args.name, "attach": args.attach, "width": w, "height": h, "fps": args.fps}
            if args.position:
                msg["position"] = args.position
            if args.look_at:
                msg["look_at"] = [float(v) for v in args.look_at] if len(args.look_at) == 3 else args.look_at[0]
            for key in ("kbps", "fov", "lifetime"):
                if getattr(args, key) is not None:
                    msg[key] = getattr(args, key)
        else:
            msg = {"cmd": "remove_camera", "name": args.name}
        reply = send(args.host, args.control_port, msg)
    except OSError as exc:
        sys.exit(f"Cannot reach producer {args.host}:{args.control_port}: {exc}")
    except KeyboardInterrupt:
        return

    if not reply.get("ok"):
        sys.exit(f"error: {reply.get('error')}")
    if "channel" in reply:
        print("added:")
        print_channels([reply["channel"]])
    else:
        print("ok")


if __name__ == "__main__":
    main()
