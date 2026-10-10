#!/usr/bin/env python3
"""Render a flashfind-vs cast (asciinema v2) to a PNG and, optionally, an MP4.

  python3 render.py run.cast out.png [--mp4 out.mp4] [--fps 20] [--font F.ttf]

The PNG is the screen at the --marker (default "summary"; also "race") (or the last frame). Needs
`pip install pyte pillow`; the MP4 also needs ffmpeg on PATH.
"""
import json, os, subprocess, sys
import pyte
from PIL import Image, ImageDraw, ImageFont

args = sys.argv[1:]
def opt(k, d=None):
    if k in args:
        i = args.index(k); v = args[i + 1]; del args[i:i + 2]; return v
    return d
mp4, fps = opt("--mp4"), float(opt("--fps", "20"))
mark = opt("--marker", "summary")
font_path = opt("--font")
cast, out = args[0], args[1]

lines = open(cast).read().splitlines()
head = json.loads(lines[0]); events = [json.loads(l) for l in lines[1:]]
W, H = head["width"], head["height"]

def find_font(bold):
    cands = [font_path] if font_path else []
    cands += [f"/usr/share/fonts/truetype/sand-box/google/JetBrains Mono/{'JetBrainsMono-Bold.ttf' if bold else 'JetBrainsMono-Regular.ttf'}",
              f"/usr/share/fonts/truetype/dejavu/DejaVuSansMono{'-Bold' if bold else ''}.ttf",
              "/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/SFNSMono.ttf"]
    for c in cands:
        if c and os.path.exists(c):
            return c
    raise SystemExit("no monospace font found; pass --font")

def load_font(bold):
    p = find_font(bold)
    if p.endswith("VariableFont_wght.ttf"):
        f = ImageFont.truetype(p, 22)
        try: f.set_variation_by_name("Bold" if bold else "Regular")
        except Exception: pass
        return f
    return ImageFont.truetype(p, 22)

# JetBrains Mono ships as a variable font on some systems.
jb = "/usr/share/fonts/truetype/sand-box/google/JetBrains Mono"
if not font_path and os.path.isdir(jb):
    var = [f for f in os.listdir(jb) if f.endswith(".ttf") and "Italic" not in f]
    if var and not os.path.exists(os.path.join(jb, "JetBrainsMono-Regular.ttf")):
        font_path = os.path.join(jb, var[0])
REG, BLD = load_font(False), load_font(True)
FALLBACK = ImageFont.truetype(next(p for p in ["/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", "/System/Library/Fonts/Menlo.ttc"] if os.path.exists(p)), 22)
def has_glyph(f, c):
    try:
        return f.getmask(c).getbbox() is not None and f.getmask(c).size != f.getmask("\uffff").size or c.isascii()
    except Exception:
        return c.isascii()
cw = round(REG.getlength("M")); ch = round(22 * 1.32)
BG, FG = (20, 20, 20), (235, 235, 240)
NAMED = {"black": (29, 31, 33), "red": (204, 102, 102), "green": (181, 189, 104), "brown": (240, 198, 116), "yellow": (240, 198, 116),
         "blue": (129, 162, 190), "magenta": (178, 148, 187), "cyan": (138, 190, 183), "white": (197, 200, 198)}
PAD, MARGIN, BAR = 36, 44, 40

def color(c, default):
    if c == "default": return default
    if c in NAMED: return NAMED[c]
    try: return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))
    except Exception: return default

def draw(screen):
    iw, ih = W * cw + 2 * PAD, H * ch + 2 * PAD + BAR
    img = Image.new("RGB", (iw + 2 * MARGIN, ih + 2 * MARGIN), (43, 45, 66))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([MARGIN, MARGIN, MARGIN + iw, MARGIN + ih], 14, fill=BG)
    for i, c in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        x = MARGIN + 24 + i * 22; y = MARGIN + 20
        d.ellipse([x - 6, y - 6, x + 6, y + 6], fill=c)
    ox, oy = MARGIN + PAD, MARGIN + PAD + BAR
    for y in range(H):
        row = screen.buffer[y]
        for x in range(W):
            c = row[x]
            if c.data in (" ", ""): 
                if c.bg != "default":
                    d.rectangle([ox + x * cw, oy + y * ch, ox + (x + 1) * cw, oy + (y + 1) * ch], fill=color(c.bg, BG))
                continue
            fg = color(c.fg, FG)
            if c.bg != "default":
                d.rectangle([ox + x * cw, oy + y * ch, ox + (x + 1) * cw, oy + (y + 1) * ch], fill=color(c.bg, BG))
            if c.data in "█▁▂▃▄▅▆▇░":
                lv = {"█": 8, "▁": 1, "▂": 2, "▃": 3, "▄": 4, "▅": 5, "▆": 6, "▇": 7}.get(c.data)
                if lv is None:  # light shade
                    d.rectangle([ox + x * cw, oy + y * ch + 6, ox + (x + 1) * cw - 1, oy + (y + 1) * ch - 6], fill=tuple(v // 3 for v in fg))
                else:
                    top = oy + (y + 1) * ch - 3 - (ch - 6) * lv // 8
                    d.rectangle([ox + x * cw, top, ox + (x + 1) * cw, oy + (y + 1) * ch - 3], fill=fg)
                continue
            f = BLD if c.bold else REG
            if not c.data.isascii() and c.data not in "·…→×µ%":
                f = FALLBACK
            d.text((ox + x * cw, oy + y * ch + 3), c.data, font=f, fill=fg)
    return img

screen = pyte.Screen(W, H); stream = pyte.Stream(screen)
snap = None
frames_dir = None
if mp4:
    frames_dir = out + ".frames"; os.makedirs(frames_dir, exist_ok=True)
next_t, k = 0.0, 0
for t, kind, data in events:
    if mp4:
        while next_t <= t:
            draw(screen).save(f"{frames_dir}/{k:06d}.png"); k += 1; next_t += 1 / fps
    if kind == "o":
        stream.feed(data)
    elif kind == "m" and data == mark and snap is None:
        snap = draw(screen)
(snap or draw(screen)).save(out)
print("wrote", out)
if mp4:
    for _ in range(int(fps * 3)):
        draw(screen).save(f"{frames_dir}/{k:06d}.png"); k += 1
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-framerate", str(fps), "-i", f"{frames_dir}/%06d.png", "-pix_fmt", "yuv420p", "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", mp4], check=True)
    subprocess.run(["rm", "-rf", frames_dir])
    print("wrote", mp4)
