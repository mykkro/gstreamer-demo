"""Shared helpers for the producer / receiver demo.

Control protocol: newline-delimited JSON over TCP.
  -> {"cmd": "list"}    <- {"ok": true, "source": ..., "channels": [...]}
  -> {"cmd": "stats"}   <- {"ok": true, "clients": {"720p30": 2, ...}}
"""
import json
import socket

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstVideo", "1.0")
from gi.repository import Gst, GstVideo  # noqa: E402,F401

DEFAULT_CONTROL_PORT = 5000


def init_gst():
    if not Gst.is_initialized():
        Gst.init(None)


def has_element(name):
    return Gst.ElementFactory.find(name) is not None


def request(host, port, cmd, timeout=3.0, **kwargs):
    """Send one JSON command to the producer's control port and return the reply."""
    msg = dict(cmd=cmd, **kwargs)
    with socket.create_connection((host, port), timeout=timeout) as s:
        s.sendall((json.dumps(msg) + "\n").encode())
        with s.makefile("r", encoding="utf-8") as f:
            line = f.readline()
    if not line:
        raise ConnectionError("producer closed control connection without reply")
    return json.loads(line)
