#!/usr/bin/env python3
# file: scripts/generate_test_media.py
# version: 1.2.0
# guid: 7b0c2a9e-3f5d-4a6b-9c8d-1e2f3a4b5c6d
# last-edited: 2026-07-31

"""
Generate small test media files for transcodarr using ffmpeg/ffprobe.

Outputs are written to ./testdata/ (gitignored).

Creates:
- test_color_720p_h264_aac.mp4 (3s, color pattern, metadata)
- test_bars_480p_h265_aac.mkv (3s, SMPTE bars)
- test_audio_sine_aac.m4a (3s, audio-only AAC)
- test_with_subs_h264_aac.mp4 (3s, embeds simple subtitles)

Requirements:
- ffmpeg and ffprobe available on PATH
"""

import shutil
import subprocess
import sys
from pathlib import Path


def check_tool(name: str) -> bool:
    return shutil.which(name) is not None


def run(cmd: list[str]) -> None:
    print("$", " ".join(cmd))
    subprocess.run(cmd, check=True)


def ensure_outdir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)


def make_color_mp4(out: Path) -> None:
    # 3s color source, 1280x720, h264+aac, with some metadata
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=1280x720:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=1000:duration=3",
        "-t",
        "3",
        "-map_metadata",
        "-1",
        "-metadata",
        "title=Color Pattern",
        "-metadata",
        "artist=Transcodarr",
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-movflags",
        "use_metadata_tags",
        str(out),
    ]
    run(cmd)


def make_bars_mkv(out: Path) -> None:
    # 3s SMPTE bars, 640x480, h265+aac
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "smptebars=size=640x480:rate=25",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=500:duration=3",
        "-t",
        "3",
        "-c:v",
        "libx265",
        "-c:a",
        "aac",
        "-b:a",
        "160k",
        str(out),
    ]
    run(cmd)


def make_audio_only(out: Path) -> None:
    # 3s sine wave to AAC in M4A
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=3",
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        str(out),
    ]
    run(cmd)


def make_with_subs(out: Path, tmpdir: Path) -> None:
    # Create a minimal SRT file
    srt = tmpdir / "temp.srt"
    srt.write_text(
        """1
00:00:00,000 --> 00:00:01,500
Hello, subtitles!

2
00:00:01,600 --> 00:00:03,000
Short test clip.
""",
        encoding="utf-8",
    )

    # Basic video/audio, then mux subtitle stream
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=854x480:rate=30",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=800:duration=3",
        "-i",
        str(srt),
        "-t",
        "3",
        "-c:v",
        "libx264",
        "-c:a",
        "aac",
        "-vf",
        "format=yuv420p",
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-map",
        "2:0",
        "-c:s",
        "mov_text",
        "-metadata:s:s:0",
        "language=eng",
        str(out),
    ]
    run(cmd)


def main() -> int:
    if not (check_tool("ffmpeg") and check_tool("ffprobe")):
        print("ERROR: ffmpeg and/or ffprobe not found on PATH.")
        print("Install ffmpeg (macOS: brew install ffmpeg) and rerun.")
        return 1

    outdir = Path(__file__).resolve().parent.parent / "testdata"
    tmpdir = outdir / "_tmp"
    ensure_outdir(outdir)
    ensure_outdir(tmpdir)

    try:
        make_color_mp4(outdir / "test_color_720p_h264_aac.mp4")
        make_bars_mkv(outdir / "test_bars_480p_h265_aac.mkv")
        make_audio_only(outdir / "test_audio_sine_aac.m4a")
        make_with_subs(outdir / "test_with_subs_h264_aac.mp4", tmpdir)
        print(f"\n✅ Generated test media in: {outdir}")
        return 0
    finally:
        # Clean temp
        if tmpdir.exists():
            for p in tmpdir.iterdir():
                try:
                    p.unlink()
                except Exception:
                    pass
            try:
                tmpdir.rmdir()
            except Exception:
                pass


if __name__ == "__main__":
    sys.exit(main())
