#!/usr/bin/env python3
"""Builds the website's video files from a render of make_video.py.

    python3 website/video/publish.py ~/Videos/meersh-tour.mp4

writes into website/public/video/:

- meersh-tour.webm, AV1 and Opus, small and sharp, for the browsers that play it (the
  page lists it first)
- meersh-tour.mp4, H.264 and AAC with fast start, for the others (Safari on older Macs
  and iPhones)
- meersh-tour-poster.webp and .jpg, the frame the player shows before it plays, from the
  stills make_video.py keeps (--stills)

and puts a hash of each file into its links in index.html (?v=...).

Needs ffmpeg with libsvtav1, libopus and libopenh264, and Pillow.
"""

import argparse
import hashlib
import re
import subprocess
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
WEBSITE = HERE.parent
OUT = WEBSITE / "public" / "video"
PAGE = WEBSITE / "public" / "index.html"
POSTER = "actor"


def ffmpeg(*args: str):
    subprocess.run(["ffmpeg", "-v", "error", "-stats", "-y", *args], check=True)


def stamp():
    """nginx lets browsers keep the videos for a day, so a new take under the same link
    would not show. The hash in the link changes with every take."""
    page = PAGE.read_text()
    for path in sorted(OUT.iterdir()):
        digest = hashlib.sha256(path.read_bytes()).hexdigest()[:10]
        page = re.sub(rf"(/video/{re.escape(path.name)})(\?v=[0-9a-f]+)?(?=[\"'])", rf"\g<1>?v={digest}", page)
    PAGE.write_text(page)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("video", type=Path, help="the video from make_video.py")
    p.add_argument("--stills", type=Path, default=Path.home() / ".cache" / "meersh-video" / "stills",
                   help="the stills folder of make_video.py's work folder")
    p.add_argument("--poster", default=POSTER, help=f"the shot whose still is the poster ({POSTER})")
    p.add_argument("--mp4-rate", default="2000k", help="the MP4's video bit rate (2000k)")
    p.add_argument("--no-webm", action="store_true", help="skip the AV1 version, which takes a few minutes")
    args = p.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    src = args.video.expanduser()

    still = Image.open(args.stills.expanduser() / f"{args.poster}.png").convert("RGB")
    still.save(OUT / "meersh-tour-poster.webp", quality=82, method=6)
    still.save(OUT / "meersh-tour-poster.jpg", quality=84, optimize=True, progressive=True)

    # OpenH264's quality mode spends the bits where the screen changes.
    ffmpeg("-i", str(src), "-c:v", "libopenh264", "-profile:v", "high", "-rc_mode", "quality",
           "-b:v", args.mp4_rate, "-maxrate", "6M", "-g", "120", "-pix_fmt", "yuv420p",
           "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart", str(OUT / "meersh-tour.mp4"))
    if not args.no_webm:
        # SVT-AV1 keeps the small text of the terminal sharp at a fraction of the size.
        ffmpeg("-i", str(src), "-c:v", "libsvtav1", "-preset", "5", "-crf", "36", "-g", "240",
               "-pix_fmt", "yuv420p10le", "-svtav1-params", "tune=0",
               "-c:a", "libopus", "-b:a", "96k", str(OUT / "meersh-tour.webm"))
    stamp()
    for path in sorted(OUT.iterdir()):
        print(f"{path.stat().st_size / 1e6:8.2f} MB  {path.relative_to(WEBSITE)}")


if __name__ == "__main__":
    main()
