"""Re-record docs/demo.gif, docs/demo.mp4, docs/og.png and the PNG icons from the page.

Run from the repo root: uv run --with playwright python docs/tools/record.py
Needs Microsoft Edge (headless, no window) and ffmpeg on PATH. Steps renderAt(t) frame by frame,
so every run gives the same frames.
"""

import pathlib
import shutil
import subprocess
import tempfile

from playwright.sync_api import sync_playwright

DOCS = pathlib.Path(__file__).resolve().parent.parent
FPS = 30
GIF_FPS = 20
GIF_WIDTH = 900
MP4_WIDTH = 1280


def page_url(query: str) -> str:
    return (DOCS / "index.html").as_uri() + query


def capture_frames(browser, out: pathlib.Path) -> None:
    page = browser.new_page(viewport={"width": 900, "height": 720}, device_scale_factor=2, color_scheme="light")
    page.goto(page_url("?record=1"))
    page.wait_for_timeout(300)
    duration = page.evaluate("DEMO.duration")
    figure = page.locator("#figure")
    count = round(duration * FPS)
    for i in range(count):
        page.evaluate(f"renderAt({i / FPS})")
        figure.screenshot(path=str(out / f"f{i:05d}.png"))
        if i % 150 == 0:
            print(f"frame {i}/{count}")
    page.close()


def capture_og(browser) -> None:
    page = browser.new_page(viewport={"width": 1200, "height": 630}, color_scheme="light")
    page.goto(page_url("?og=1&still=1&t=4.6"))
    page.wait_for_timeout(300)
    page.screenshot(path=str(DOCS / "og.png"))
    page.close()


def capture_icons(browser) -> None:
    logo = (DOCS / "icon.svg").read_text(encoding="utf-8")
    for name, size in (("favicon.png", 64), ("apple-touch-icon.png", 180)):
        page = browser.new_page(viewport={"width": size, "height": size}, color_scheme="light")
        pad = round(size * 0.16)
        page.set_content(
            "<style>html,body{margin:0}div{width:%dpx;height:%dpx;box-sizing:border-box;padding:%dpx;"
            "background:#fff;border-radius:%dpx}svg{width:100%%;height:100%%;display:block}</style><div>%s</div>"
            % (size, size, pad, round(size * 0.22), logo)
        )
        page.locator("div").screenshot(path=str(DOCS / name), omit_background=True)
        page.close()


def encode(frames: pathlib.Path) -> None:
    pattern = str(frames / "f%05d.png")
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-framerate", str(FPS), "-i", pattern,
         "-vf", f"scale={MP4_WIDTH}:-2:flags=lanczos", "-c:v", "libx264", "-preset", "slow", "-crf", "24",
         "-tune", "animation", "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-an", str(DOCS / "demo.mp4")],
        check=True,
    )
    graph = (
        f"fps={GIF_FPS},scale={GIF_WIDTH}:-1:flags=lanczos,split[a][b];"
        "[a]palettegen=max_colors=128:stats_mode=full[p];"
        "[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle"
    )
    subprocess.run(
        ["ffmpeg", "-v", "error", "-y", "-framerate", str(FPS), "-i", pattern,
         "-filter_complex", graph, "-loop", "0", str(DOCS / "demo.gif")],
        check=True,
    )


def main() -> None:
    frames = pathlib.Path(tempfile.mkdtemp(prefix="igorio-demo-"))
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(channel="msedge", headless=True)
            capture_icons(browser)
            capture_og(browser)
            capture_frames(browser, frames)
            browser.close()
        encode(frames)
    finally:
        shutil.rmtree(frames, ignore_errors=True)
    for name in ("demo.gif", "demo.mp4", "og.png"):
        print(name, (DOCS / name).stat().st_size // 1024, "KB")


if __name__ == "__main__":
    main()
