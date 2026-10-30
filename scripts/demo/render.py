#!/usr/bin/env python3
"""Turn a recording from record.py into PNG frames with captions.

Usage: render.py REC_DIR FRAME_DIR. The fonts are macOS's (SF Mono, SF Pro).

Each caption goes where the screen stays empty for as long as it shows: the cells
of every captured frame in its time span are marked as occupied, grown by a cell
of margin, and the caption takes the free spot nearest the lower middle of the
terminal, keeping the previous caption's spot when that one is still free. Key
presses show as keycaps in the title bar, which covers nothing.
"""

import json
import math
import pathlib
import re
import sys

from PIL import Image, ImageDraw, ImageFont

FONT_SIZE = 16
MONO = "/System/Applications/Utilities/Terminal.app/Contents/Resources/Fonts/SF-Mono-{}.otf"
SANS = "/System/Library/Fonts/SFNS.ttf"
FALLBACK = "/System/Library/Fonts/Menlo.ttc"

BG = (20, 22, 27)
FG = (215, 218, 224)
CANVAS = (35, 39, 47)
TITLE_BAR = (43, 47, 56)
PALETTE16 = [
    (29, 32, 38), (224, 108, 117), (152, 195, 121), (229, 192, 123),
    (97, 175, 239), (198, 120, 221), (86, 182, 194), (171, 178, 191),
    (92, 99, 112), (255, 123, 134), (181, 232, 144), (255, 214, 138),
    (140, 200, 255), (222, 155, 240), (127, 212, 223), (230, 230, 230),
]
ACCENT = (232, 163, 61)

MARGIN, BAR_H, PAD = 24, 30, 10


def xterm256(n):
    if n < 16:
        return PALETTE16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return steps[n // 36], steps[n // 6 % 6], steps[n % 6]
    v = 8 + (n - 232) * 10
    return v, v, v


SGR = re.compile(r"\x1b\[([0-9;:]*)m")


class Cell:
    __slots__ = ("ch", "fg", "bg", "bold", "dim", "rev", "ul")

    def __init__(self, ch=" ", fg=None, bg=None, bold=False, dim=False, rev=False, ul=False):
        self.ch, self.fg, self.bg = ch, fg, bg
        self.bold, self.dim, self.rev, self.ul = bold, dim, rev, ul


def parse_line(line, cols):
    cells, state = [], dict(fg=None, bg=None, bold=False, dim=False, rev=False, ul=False)
    pos = 0
    for m in SGR.finditer(line + "\x1b[m"):
        for ch in line[pos:m.start()]:
            cells.append(Cell(ch, **state))
        pos = m.end()
        params = [int(p) if p else 0 for p in re.split("[;:]", m.group(1))] or [0]
        i = 0
        while i < len(params):
            p = params[i]
            if p == 0:
                state.update(fg=None, bg=None, bold=False, dim=False, rev=False, ul=False)
            elif p == 1:
                state["bold"] = True
            elif p == 2:
                state["dim"] = True
            elif p == 4:
                state["ul"] = True
            elif p == 7:
                state["rev"] = True
            elif p == 22:
                state.update(bold=False, dim=False)
            elif p == 24:
                state["ul"] = False
            elif p == 27:
                state["rev"] = False
            elif 30 <= p <= 37:
                state["fg"] = PALETTE16[p - 30]
            elif 90 <= p <= 97:
                state["fg"] = PALETTE16[p - 90 + 8]
            elif 40 <= p <= 47:
                state["bg"] = PALETTE16[p - 40]
            elif 100 <= p <= 107:
                state["bg"] = PALETTE16[p - 100 + 8]
            elif p == 39:
                state["fg"] = None
            elif p == 49:
                state["bg"] = None
            elif p in (38, 48):
                which = "fg" if p == 38 else "bg"
                if params[i + 1] == 5:
                    state[which] = xterm256(params[i + 2])
                    i += 2
                elif params[i + 1] == 2:
                    # 38;2;r;g;b, or 38:2::r:g:b with an empty colour space id
                    rgb = params[i + 2:i + 5]
                    if len(params) > i + 5 and params[i + 2] == 0 and ":" in m.group(1):
                        rgb = params[i + 3:i + 6]
                    state[which] = tuple(rgb)
                    i += len(rgb) + 1
            i += 1
    cells = cells[:cols]
    return cells + [Cell() for _ in range(cols - len(cells))]


class Fonts:
    def __init__(self):
        self.regular = ImageFont.truetype(MONO.format("Regular"), FONT_SIZE)
        self.bold = ImageFont.truetype(MONO.format("Bold"), FONT_SIZE)
        self.fallback = ImageFont.truetype(FALLBACK, FONT_SIZE)
        self.cw = round(self.regular.getlength("M"))
        ascent, descent = self.regular.getmetrics()
        self.ch = round((ascent + descent) * 1.08)
        self.baseline = (self.ch - ascent - descent) // 2 + ascent
        self.notdef = self.glyph(self.regular, "\U000F0000")
        self.missing = {}

    def glyph(self, font, ch):
        im = Image.new("L", (self.cw * 2, self.ch * 2))
        ImageDraw.Draw(im).text((0, 0), ch, font=font, fill=255)
        return im.tobytes()

    def font_for(self, ch, bold):
        """SF Mono, or Menlo for what SF Mono lacks."""
        if ord(ch) > 0x7F:
            if ch not in self.missing:
                self.missing[ch] = self.glyph(self.regular, ch) == self.notdef
            if self.missing[ch]:
                return self.fallback
        return self.bold if bold else self.regular


# Light box drawing: which arms each character has (up, down, left, right), and dashed
BOX = {
    "─": "lr", "│": "ud", "┌": "dr", "┐": "dl", "└": "ur", "┘": "ul", "├": "udr",
    "┤": "udl", "┬": "dlr", "┴": "ulr", "┼": "udlr", "╭": "dr", "╮": "dl", "╰": "ur",
    "╯": "ul", "╴": "l", "╶": "r", "╵": "u", "╷": "d",
}
DASHED = {"┄": "lr", "┈": "lr", "╌": "lr", "┆": "ud", "┊": "ud", "╎": "ud"}


def draw_special(d, ch, x, y, cw, chh, color):
    """Box drawing, block elements and braille as shapes, so they join up."""
    code = ord(ch)
    cx, cy = x + cw // 2, y + chh // 2
    if ch in BOX or ch in DASHED:
        arms = BOX.get(ch) or DASHED[ch]
        ends = {"u": (cx, y), "d": (cx, y + chh), "l": (x, cy), "r": (x + cw, cy)}
        for arm in arms:
            ex, ey = ends[arm]
            if ch in DASHED:
                n = 3
                for k in range(n):
                    a, b = k / n, (k + 0.55) / n
                    if arm in "ud":
                        d.line([(cx, y + chh * a), (cx, y + chh * b)], fill=color, width=1)
                    else:
                        d.line([(x + cw * a, cy), (x + cw * b, cy)], fill=color, width=1)
                break
            d.line([(cx, cy), (ex, ey)], fill=color, width=1)
        return True
    if 0x2581 <= code <= 0x2588:  # ▁ to █: eighths from the bottom
        h = round(chh * (code - 0x2580) / 8)
        d.rectangle([x, y + chh - h, x + cw - 1, y + chh - 1], fill=color)
        return True
    if 0x2800 <= code <= 0x28FF:  # braille: two columns of four dots
        bits = code - 0x2800
        dots = [(0, 0, 0x01), (0, 1, 0x02), (0, 2, 0x04), (1, 0, 0x08), (1, 1, 0x10),
                (1, 2, 0x20), (0, 3, 0x40), (1, 3, 0x80)]
        r = max(1.0, cw / 7)
        for col, row, bit in dots:
            if bits & bit:
                px = x + cw * (0.3 + 0.4 * col)
                py = y + chh * (0.14 + 0.24 * row)
                d.ellipse([px - r, py - r, px + r, py + r], fill=color)
        return True
    return False


def blend(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


class Terminal:
    def __init__(self, fonts, cols, rows):
        self.f, self.cols, self.rows = fonts, cols, rows
        self.w, self.h = cols * fonts.cw, rows * fonts.ch

    def draw(self, grid, cursor):
        img = Image.new("RGB", (self.w, self.h), BG)
        d = ImageDraw.Draw(img)
        cw, chh = self.f.cw, self.f.ch
        cx, cy, shown = cursor
        for r, row in enumerate(grid):
            for c, cell in enumerate(row):
                fg, bg = cell.fg or FG, cell.bg or BG
                if cell.rev:
                    fg, bg = bg, fg
                if shown and (c, r) == (cx, cy):
                    fg, bg = BG, FG
                if cell.dim:
                    fg = blend(fg, bg, 0.45)
                x, y = c * cw, r * chh
                if bg != BG:
                    d.rectangle([x, y, x + cw - 1, y + chh - 1], fill=bg)
                if cell.ch != " " and not draw_special(d, cell.ch, x, y, cw, chh, fg):
                    font = self.f.font_for(cell.ch, cell.bold)
                    d.text((x, y + self.f.baseline), cell.ch, font=font, fill=fg, anchor="ls")
                if cell.ul:
                    d.line([(x, y + chh - 2), (x + cw - 1, y + chh - 2)], fill=fg)
        return img


def occupied(grid):
    return [[cell.ch != " " or cell.bg is not None or cell.rev for cell in row] for row in grid]


# Captions
CAPTION_FONT = ImageFont.truetype(SANS, 19)
try:
    CAPTION_FONT.set_variation_by_name("Medium")
except Exception:
    pass
KEY_FONT = ImageFont.truetype(MONO.format("Bold"), 15)
TOKEN = re.compile(r"\[([^\]]+)\]")


def tokens(text):
    """Plain words and [key] keycaps, in order."""
    out, pos = [], 0
    for m in TOKEN.finditer(text):
        out += [("w", w) for w in text[pos:m.start()].split()]
        out.append(("k", m.group(1)))
        pos = m.end()
    return out + [("w", w) for w in text[pos:].split()]


def token_width(tok):
    kind, s = tok
    if kind == "w":
        return CAPTION_FONT.getlength(s)
    return KEY_FONT.getlength(s) + 16


def layout(text, max_w):
    """Lines of tokens no wider than max_w."""
    space = CAPTION_FONT.getlength(" ")
    lines, line, w = [], [], 0
    for tok in tokens(text):
        tw = token_width(tok)
        if line and w + space + tw > max_w:
            lines.append(line)
            line, w = [], 0
        w += (space if line else 0) + tw
        line.append(tok)
    return lines + [line]


LINE_H = 30
BOX_PAD_X, BOX_PAD_Y = 18, 10


def caption_size(lines):
    space = CAPTION_FONT.getlength(" ")
    widths = [sum(token_width(t) for t in line) + space * (len(line) - 1) for line in lines]
    return math.ceil(max(widths)) + 2 * BOX_PAD_X + 4, len(lines) * LINE_H + 2 * BOX_PAD_Y


def keycap(d, x, y, label, alpha):
    w = KEY_FONT.getlength(label) + 12
    d.rounded_rectangle([x, y, x + w, y + 22], radius=5, fill=(58, 64, 76, alpha),
                        outline=(110, 118, 134, alpha))
    d.text((x + 6, y + 11), label, font=KEY_FONT, fill=(240, 242, 246, alpha), anchor="lm")
    return w


def draw_caption(layer, lines, box, alpha):
    d = ImageDraw.Draw(layer)
    x0, y0, x1, y1 = box
    a = round(alpha * 238)
    d.rounded_rectangle(box, radius=10, fill=(12, 14, 19, a), outline=(255, 255, 255, round(alpha * 45)))
    d.rounded_rectangle([x0, y0, x0 + 4, y1], radius=2, fill=(*ACCENT, round(alpha * 255)))
    space = CAPTION_FONT.getlength(" ")
    for i, line in enumerate(lines):
        x = x0 + BOX_PAD_X + 4
        cy = y0 + BOX_PAD_Y + i * LINE_H + LINE_H // 2
        for j, (kind, s) in enumerate(line):
            if j:
                x += space
            if kind == "w":
                d.text((x, cy), s, font=CAPTION_FONT, fill=(244, 246, 250, round(alpha * 255)), anchor="lm")
                x += CAPTION_FONT.getlength(s)
            else:
                x += keycap(d, x + 2, cy - 11, s, round(alpha * 255)) + 4


def free_spot(occ, need_c, need_r, prefer=None):
    """The top-left cell of a need_c x need_r block of free cells: prefer's if it is
    free, else the one nearest the lower middle. Without a free block, the one that
    covers the fewest occupied cells."""
    rows, cols = len(occ), len(occ[0])
    # summed-area table of occupied cells
    sat = [[0] * (cols + 1) for _ in range(rows + 1)]
    for r in range(rows):
        for c in range(cols):
            sat[r + 1][c + 1] = occ[r][c] + sat[r][c + 1] + sat[r + 1][c] - sat[r][c]

    def free(r, c):
        r2, c2 = r + need_r, c + need_c
        return sat[r2][c2] - sat[r][c2] - sat[r2][c] + sat[r][c] == 0

    if prefer and prefer[0] + need_r <= rows and prefer[1] + need_c <= cols and free(*prefer):
        return prefer, 0
    best, best_score = None, None
    for r in range(rows - need_r + 1):
        for c in range(cols - need_c + 1):
            r2, c2 = r + need_r, c + need_c
            covered = sat[r2][c2] - sat[r][c2] - sat[r2][c] + sat[r][c]
            dx = abs(c + need_c / 2 - cols / 2) / cols
            dy = abs(r + need_r / 2 - rows * 0.72) / rows
            score = (covered, dx + 1.4 * dy)
            if best_score is None or score < best_score:
                best, best_score = (r, c), score
    return best, best_score[0]


def grow(occ, by=1):
    rows, cols = len(occ), len(occ[0])
    out = [[False] * cols for _ in range(rows)]
    for r in range(rows):
        for c in range(cols):
            if occ[r][c]:
                for rr in range(max(0, r - by), min(rows, r + by + 1)):
                    for cc in range(max(0, c - by), min(cols, c + by + 1)):
                        out[rr][cc] = True
    return out


def main():
    rec, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    for old in out.glob("*.png"):
        old.unlink()
    captures = [json.loads(line) for line in open(rec / "frames.jsonl")]
    events = json.loads((rec / "events.json").read_text())
    fonts = Fonts()
    meta = json.loads((rec / "meta.json").read_text())
    cols, rows, fps = meta["cols"], meta["rows"], meta["fps"]
    term = Terminal(fonts, cols, rows)
    grids = {}

    def grid_of(cap):
        key = "\n".join(cap["screen"])
        if key not in grids:
            grids[key] = [parse_line(l, cols) for l in cap["screen"]]
        return grids[key]

    end = next(e["t"] for e in events if e["kind"] == "end")
    # Captions: (start, end, text)
    spans, current = [], None
    for e in events:
        if e["kind"] in ("say", "hush", "end"):
            if current:
                spans.append((current[0], e["t"], current[1]))
            current = (e["t"], e["text"]) if e["kind"] == "say" else None
    keys = [(e["t"], e["key"]) for e in events if e["kind"] == "key"]

    # Place every caption
    placed, prefer = [], None
    for start, stop, text in spans:
        occ = [[False] * cols for _ in range(rows)]
        for cap in captures:
            # the first moments fade in over the view the last key is leaving
            if start + 0.15 <= cap["t"] <= stop:
                for r, row in enumerate(occupied(grid_of(cap))):
                    for c, v in enumerate(row):
                        occ[r][c] |= v
                x, y, shown = cap["cursor"]
                if shown and y < rows and x < cols:
                    occ[y][x] = True
        # One line if it fits anywhere, else narrower and taller; then the same
        # without a margin; failing all, the form and spot that cover least
        tries = []
        for margin in (1, 0):
            grown = grow(occ, margin)
            for max_w in (term.w * 0.78, term.w * 0.5, term.w * 0.36):
                lines = layout(text, max_w)
                w, h = caption_size(lines)
                need_c, need_r = math.ceil(w / fonts.cw), math.ceil(h / fonts.ch)
                # The previous caption's spot, if this one is the same size and free
                same = prefer if prefer and prefer[2] == (need_c, need_r) else None
                spot, covered = free_spot(grown, need_c, need_r, same and same[:2])
                tries.append((covered, lines, w, h, need_c, need_r, spot))
                if covered == 0:
                    break
            if tries[-1][0] == 0:
                break
        covered, lines, w, h, need_c, need_r, spot = min(tries, key=lambda t: t[0])
        if covered:
            print(f"no free spot for {text!r}; it covers {covered} cells", file=sys.stderr)
        r, c = spot
        # centre the box in its block of cells
        bx = c * fonts.cw + (need_c * fonts.cw - w) // 2
        by = r * fonts.ch + (need_r * fonts.ch - h) // 2
        placed.append((start, stop, lines, (bx, by, bx + w, by + h)))
        prefer = (r, c, (need_c, need_r))
        print(f"{start:6.1f}-{stop:6.1f}  row {r:2} col {c:3}  {text}", file=sys.stderr)

    win_w, win_h = term.w + 2 * PAD, term.h + 2 * PAD + BAR_H
    canvas_w, canvas_h = win_w + 2 * MARGIN, win_h + 2 * MARGIN
    canvas_w += canvas_w % 2
    canvas_h += canvas_h % 2
    title_font = ImageFont.truetype(SANS, 14)
    term_cache = {}
    ci = 0
    n = int(end * fps) + 1
    for i in range(n):
        t = i / fps
        while ci + 1 < len(captures) and captures[ci + 1]["t"] <= t:
            ci += 1
        cap = captures[ci]
        key = ("\n".join(cap["screen"]), tuple(cap["cursor"]))
        if key not in term_cache:
            term_cache.clear()
            term_cache[key] = term.draw(grid_of(cap), cap["cursor"])
        frame = Image.new("RGB", (canvas_w, canvas_h), CANVAS)
        wx, wy = MARGIN, MARGIN
        fd = ImageDraw.Draw(frame)
        fd.rounded_rectangle([wx, wy, wx + win_w, wy + win_h], radius=10, fill=BG, outline=(60, 65, 76))
        fd.rounded_rectangle([wx, wy, wx + win_w, wy + BAR_H], radius=10, fill=TITLE_BAR)
        fd.rectangle([wx + 1, wy + BAR_H - 10, wx + win_w - 1, wy + BAR_H], fill=TITLE_BAR)
        for k, col in enumerate([(255, 95, 87), (254, 188, 46), (40, 200, 64)]):
            ccx, ccy = wx + 18 + k * 20, wy + BAR_H // 2
            fd.ellipse([ccx - 6, ccy - 6, ccx + 6, ccy + 6], fill=col)
        fd.text((wx + win_w // 2, wy + BAR_H // 2), "mtr-ng", font=title_font, fill=(170, 176, 188), anchor="mm")
        tx, ty = wx + PAD, wy + BAR_H + PAD
        frame.paste(term_cache[key], (tx, ty))

        layer = Image.new("RGBA", frame.size, (0, 0, 0, 0))
        for start, stop, lines, (x0, y0, x1, y1) in placed:
            if start <= t < stop:
                alpha = min(1.0, (t - start) / 0.3, (stop - t) / 0.3)
                draw_caption(layer, lines, (tx + x0, ty + y0, tx + x1, ty + y1), max(0.0, alpha))
        # Key presses: keycaps at the right of the title bar, newest rightmost
        recent = [(kt, k) for kt, k in keys if kt <= t < kt + 1.4][-4:]
        kd = ImageDraw.Draw(layer)
        x = wx + win_w - 12
        for kt, k in reversed(recent):
            alpha = round(255 * min(1.0, (kt + 1.4 - t) / 0.4))
            w = KEY_FONT.getlength(k) + 12
            x -= w
            keycap(kd, x, wy + 4, k, alpha)
            x -= 6
        frame = Image.alpha_composite(frame.convert("RGBA"), layer).convert("RGB")
        frame.save(out / f"{i:05d}.png", optimize=False)
    print(f"{n} frames, {canvas_w}x{canvas_h}", file=sys.stderr)


if __name__ == "__main__":
    main()
