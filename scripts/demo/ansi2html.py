#!/usr/bin/env python3
"""Turns `tmux capture-pane -e` captures into one HTML page of screenshots.

    ansi2html.py out.html "Title one" one.ansi "Title two" two.ansi ...
    ansi2html.py --bare out.html one.ansi      (a single frame, no page chrome)

Handles the SGR codes ratatui emits: bold, dim, italic, underline, reverse,
and 16-colour, 256-colour and 24-bit foregrounds and backgrounds.
"""

import html
import re
import sys

SGR = re.compile(r"\x1b\[([0-9;]*)m")
OTHER = re.compile(r"\x1b(\[[0-9;?]*[A-Za-z]|\][^\x07]*\x07|[()][A-Za-z0-9])")
BASE16 = [
    "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
    "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
]
DEFAULT_FG, DEFAULT_BG = "#ebdbb2", "#282828"


def palette(index):
    if index < 16:
        return BASE16[index]
    if index < 232:
        index -= 16
        levels = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (levels[index // 36], levels[index // 6 % 6], levels[index % 6])
    grey = 8 + (index - 232) * 10
    return "#%02x%02x%02x" % (grey, grey, grey)


class Style:
    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False

    def apply(self, params):
        codes = [int(p) if p else 0 for p in params.split(";")] if params else [0]
        i = 0
        while i < len(codes):
            code = codes[i]
            if code == 0:
                self.reset()
            elif code == 1:
                self.bold = True
            elif code == 2:
                self.dim = True
            elif code == 3:
                self.italic = True
            elif code == 4:
                self.underline = True
            elif code == 7:
                self.reverse = True
            elif code == 22:
                self.bold = self.dim = False
            elif code == 23:
                self.italic = False
            elif code == 24:
                self.underline = False
            elif code == 27:
                self.reverse = False
            elif 30 <= code <= 37:
                self.fg = BASE16[code - 30]
            elif 90 <= code <= 97:
                self.fg = BASE16[code - 82]
            elif 40 <= code <= 47:
                self.bg = BASE16[code - 40]
            elif 100 <= code <= 107:
                self.bg = BASE16[code - 92]
            elif code == 39:
                self.fg = None
            elif code == 49:
                self.bg = None
            elif code in (38, 48) and i + 1 < len(codes):
                if codes[i + 1] == 5 and i + 2 < len(codes):
                    color = palette(codes[i + 2])
                    i += 2
                elif codes[i + 1] == 2 and i + 4 < len(codes):
                    color = "#%02x%02x%02x" % tuple(codes[i + 2 : i + 5])
                    i += 4
                else:
                    color = None
                if code == 38:
                    self.fg = color
                else:
                    self.bg = color
            i += 1

    def css(self):
        fg, bg = self.fg or DEFAULT_FG, self.bg or DEFAULT_BG
        if self.reverse:
            fg, bg = bg, fg
        rules = []
        if fg != DEFAULT_FG:
            rules.append("color:" + fg)
        if bg != DEFAULT_BG:
            rules.append("background:" + bg)
        if self.bold:
            rules.append("font-weight:700")
        if self.dim:
            rules.append("opacity:.6")
        if self.italic:
            rules.append("font-style:italic")
        if self.underline:
            rules.append("text-decoration:underline")
        return ";".join(rules)


def convert(text):
    style, out = Style(), []
    for line in text.rstrip("\n").split("\n"):
        parts, pos = [], 0
        for match in SGR.finditer(line):
            parts.append((style.css(), OTHER.sub("", line[pos : match.start()])))
            style.apply(match.group(1))
            pos = match.end()
        parts.append((style.css(), OTHER.sub("", line[pos:])))
        rendered = "".join(
            f'<span style="{css}">{html.escape(chunk)}</span>' if css else html.escape(chunk)
            for css, chunk in parts
            if chunk
        )
        # Each row is a fixed-height block, so cell backgrounds meet with no gaps.
        out.append(f'<span class="l">{rendered or " "}</span>')
    return "".join(out)


def bare(output, path):
    """One capture alone, edge to edge, for a PNG screenshot."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        body = convert(handle.read())
    with open(output, "w", encoding="utf-8") as handle:
        handle.write(
            """<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Screenshot</title>
<style>html,body{margin:0;background:#282828}
pre{margin:0;padding:0;background:#282828;color:#ebdbb2;white-space:pre;display:inline-block;
font:13px/1 Menlo,"SF Mono",monospace;font-variant-ligatures:none}
pre .l{display:block;height:1.25em;line-height:1.25em}
pre .l span{display:inline-block;height:1.25em;line-height:1.25em;vertical-align:top}
</style></head><body><pre>"""
            + body
            + "</pre></body></html>\n"
        )


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--bare":
        return bare(sys.argv[2], sys.argv[3])
    if len(sys.argv) < 4 or len(sys.argv) % 2:
        sys.exit(__doc__)
    output, pairs = sys.argv[1], sys.argv[2:]
    shots = []
    for title, path in zip(pairs[::2], pairs[1::2]):
        with open(path, encoding="utf-8", errors="replace") as handle:
            shots.append(
                f"<section><h2>{html.escape(title)}</h2>"
                f'<div class="frame"><pre>{convert(handle.read())}</pre></div></section>'
            )
    with open(output, "w", encoding="utf-8") as handle:
        handle.write(
            """<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Launcher Screenshots</title><style>
:root{--bg:#1d2021;--text:#ebdbb2;--muted:#a89984}
body{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 -apple-system,sans-serif}
section{max-width:1500px;margin:0 auto;padding:20px 24px}
h2{font-size:15px;margin:0 0 8px;color:var(--muted);font-weight:600}
.frame{overflow-x:auto;border-radius:8px;border:1px solid #3c3836}
pre{margin:0;padding:10px 12px;background:#282828;color:#ebdbb2;white-space:pre;
font:13px/1 Menlo,"SF Mono",monospace;font-variant-ligatures:none}
pre .l{display:block;height:1.25em;line-height:1.25em}
pre .l span{display:inline-block;height:1.25em;line-height:1.25em;vertical-align:top}
</style></head><body>"""
            + "".join(shots)
            + "</body></html>\n"
        )


if __name__ == "__main__":
    main()
