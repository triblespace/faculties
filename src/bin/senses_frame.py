#!/usr/bin/env python3
"""Thin frame-grab shim for the `senses` faculty.

Grabs ONE camera frame from the running Reachy Mini daemon and writes it as
a PNG to the path given as argv[1]; prints "WIDTHxHEIGHT" to stdout.

This is the single Python island in `senses`: frame pixels only flow over the
daemon's WebRTC/GStreamer pipeline, with no plain HTTP snapshot endpoint, so
pulling them natively would mean a gstreamer-rs/webrtc-rs build. Everything
else in the faculty — proprioception, motion, the pile write — is pure Rust
over the daemon's REST API. This shim is the obvious target for a native
Rust frame path once the VLA loop needs the continuous stream.

The faculty embeds this file (include_str!) and writes it to a temp path at
runtime, so there is no loose script to lose.
"""
import sys
import time

import numpy as np
from PIL import Image
from reachy_mini import ReachyMini


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: senses_frame.py <out.png>", file=sys.stderr)
        return 64
    out = sys.argv[1]

    # Context manager handles teardown (media_manager.close + client.disconnect).
    with ReachyMini(connection_mode="localhost_only", spawn_daemon=False) as mini:
        # WebRTC frames can be None for the first moment while the pipeline
        # ramps up — give it a brief window rather than failing on a cold pull.
        frame = None
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            frame = mini.media.get_frame()
            if frame is not None:
                break
            time.sleep(0.1)
        if frame is None:
            print("no frame available from daemon", file=sys.stderr)
            return 2

        # get_frame returns BGR uint8 (H, W, 3); store true-colour RGB.
        rgb = np.ascontiguousarray(frame[:, :, ::-1])
        Image.fromarray(rgb).save(out)
        h, w = frame.shape[:2]
        print(f"{w}x{h}")
        return 0


if __name__ == "__main__":
    sys.exit(main())
