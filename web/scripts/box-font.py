"""Make public/fonts/ursa-box.woff2: the box-drawing characters for the terminal.

IBM Plex Mono's web fonts have no box-drawing characters (U+2500-257F), so a
table's borders were drawn in whatever monospace font the visitor's system
fell back to, and in most of them a vertical line is shorter than a terminal
row: every border had gaps. This font has only those characters, drawn as
plain strokes on IBM Plex Mono's metrics (1000 units per em, 600 wide, ascent
1025, descent 275), with the vertical strokes running past the top and bottom
of the row so that one row's line meets the next whatever the line height.
style.css loads it for U+2500-257F only (unicode-range) ahead of Plex Mono.

Run once, when the glyphs change, with fontTools and brotli installed:

    python3 scripts/box-font.py public/fonts/ursa-box.woff2
"""

import sys

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

UPM = 1000
ADVANCE = 600
ASCENT, DESCENT = 1025, 275
# Where the horizontal strokes sit and the vertical ones cross: the middle of
# Plex Mono's line box.
MID_X = ADVANCE // 2
MID_Y = (ASCENT - DESCENT) // 2
# Light and heavy stroke widths, close to Plex Mono's regular stems.
LIGHT, HEAVY = 70, 140
# How far strokes run past the cell, so neighbouring cells and rows overlap.
OVER_X = 20
OVER_Y = 200

# Each character as the arms it draws from the centre: left, right, up, down,
# each light (1) or heavy (2). Light and heavy box drawing, U+2500-254B.
ARMS = {
    0x2500: (1, 1, 0, 0), 0x2501: (2, 2, 0, 0), 0x2502: (0, 0, 1, 1), 0x2503: (0, 0, 2, 2),
    0x250C: (0, 1, 0, 1), 0x250D: (0, 2, 0, 1), 0x250E: (0, 1, 0, 2), 0x250F: (0, 2, 0, 2),
    0x2510: (1, 0, 0, 1), 0x2511: (2, 0, 0, 1), 0x2512: (1, 0, 0, 2), 0x2513: (2, 0, 0, 2),
    0x2514: (0, 1, 1, 0), 0x2515: (0, 2, 1, 0), 0x2516: (0, 1, 2, 0), 0x2517: (0, 2, 2, 0),
    0x2518: (1, 0, 1, 0), 0x2519: (2, 0, 1, 0), 0x251A: (1, 0, 2, 0), 0x251B: (2, 0, 2, 0),
    0x251C: (0, 1, 1, 1), 0x251D: (0, 2, 1, 1), 0x2520: (0, 1, 2, 2), 0x2523: (0, 2, 2, 2),
    0x2524: (1, 0, 1, 1), 0x2525: (2, 0, 1, 1), 0x2528: (1, 0, 2, 2), 0x252B: (2, 0, 2, 2),
    0x252C: (1, 1, 0, 1), 0x252F: (2, 2, 0, 1), 0x2530: (1, 1, 0, 2), 0x2533: (2, 2, 0, 2),
    0x2534: (1, 1, 1, 0), 0x2537: (2, 2, 1, 0), 0x2538: (1, 1, 2, 0), 0x253B: (2, 2, 2, 0),
    0x253C: (1, 1, 1, 1), 0x253F: (2, 2, 1, 1), 0x2542: (1, 1, 2, 2), 0x254B: (2, 2, 2, 2),
    0x2574: (1, 0, 0, 0), 0x2575: (0, 0, 1, 0), 0x2576: (0, 1, 0, 0), 0x2577: (0, 0, 0, 1),
}


def rect(pen, x0, y0, x1, y1, xs):
    # Clockwise, as TrueType outlines are; overlapping rectangles fill as one.
    xs.append(x0)
    pen.moveTo((x0, y0))
    pen.lineTo((x0, y1))
    pen.lineTo((x1, y1))
    pen.lineTo((x1, y0))
    pen.closePath()


def glyph(arms):
    left, right, up, down = arms
    width = lambda weight: LIGHT if weight == 1 else HEAVY
    # The centre joint is as thick as the heaviest arm in each direction.
    horizontal = max(left, right)
    vertical = max(up, down)
    pen = TTGlyphPen(None)
    xs = []
    for weight, x0, x1 in ((left, -OVER_X, MID_X), (right, MID_X, ADVANCE + OVER_X)):
        if weight:
            half = width(weight) // 2
            reach = width(vertical) // 2 if vertical else half
            # Each arm runs into the centre far enough to meet a crossing arm.
            x0, x1 = (x0, MID_X + reach) if x0 < MID_X else (MID_X - reach, x1)
            rect(pen, x0, MID_Y - half, x1, MID_Y + half, xs)
    for weight, y0, y1 in ((down, -DESCENT - OVER_Y, MID_Y), (up, MID_Y, ASCENT + OVER_Y)):
        if weight:
            half = width(weight) // 2
            reach = width(horizontal) // 2 if horizontal else half
            y0, y1 = (y0, MID_Y + reach) if y0 < MID_Y else (MID_Y - reach, y1)
            rect(pen, MID_X - half, y0, MID_X + half, y1, xs)
    # The left side bearing must be the outline's left edge: renderers place
    # the outline by it.
    return pen.glyph(), min(xs)


def main(out):
    names = [".notdef"] + [f"uni{code:04X}" for code in ARMS]
    fb = FontBuilder(UPM, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap({code: f"uni{code:04X}" for code in ARMS})
    glyphs = {".notdef": (TTGlyphPen(None).glyph(), 0)}
    glyphs.update({f"uni{code:04X}": glyph(arms) for code, arms in ARMS.items()})
    fb.setupGlyf({name: g for name, (g, _) in glyphs.items()})
    fb.setupHorizontalMetrics({name: (ADVANCE, lsb) for name, (_, lsb) in glyphs.items()})
    fb.setupHorizontalHeader(ascent=ASCENT, descent=-DESCENT)
    fb.setupNameTable({"familyName": "Ursa Box", "styleName": "Regular"})
    fb.setupOS2(
        version=4,
        sTypoAscender=ASCENT,
        sTypoDescender=-DESCENT,
        sTypoLineGap=0,
        usWinAscent=ASCENT,
        usWinDescent=DESCENT,
        fsSelection=0x40 | 0x80,  # REGULAR, USE_TYPO_METRICS
    )
    fb.setupPost(isFixedPitch=1)
    fb.font.flavor = "woff2"
    fb.save(out)


if __name__ == "__main__":
    main(sys.argv[1])
