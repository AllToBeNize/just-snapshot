"""Convert Lucide SVG icons into Rust path constants (24x24 grid, M/L/C/Z only)."""
import math
import re
import sys
import xml.etree.ElementTree as ET

TOKEN = re.compile(r"[MmLlHhVvCcSsQqAaZz]|[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?")


def arc_to_cubics(x1, y1, rx, ry, phi, large, sweep, x2, y2):
    if rx == 0 or ry == 0:
        return [("L", x2, y2)]
    phi = math.radians(phi)
    cp, sp = math.cos(phi), math.sin(phi)
    dx, dy = (x1 - x2) / 2, (y1 - y2) / 2
    x1p = cp * dx + sp * dy
    y1p = -sp * dx + cp * dy
    rx, ry = abs(rx), abs(ry)
    lam = x1p**2 / rx**2 + y1p**2 / ry**2
    if lam > 1:
        s = math.sqrt(lam)
        rx, ry = rx * s, ry * s
    num = rx**2 * ry**2 - rx**2 * y1p**2 - ry**2 * x1p**2
    den = rx**2 * y1p**2 + ry**2 * x1p**2
    coef = math.sqrt(max(0, num / den)) if den else 0
    if large == sweep:
        coef = -coef
    cxp = coef * rx * y1p / ry
    cyp = -coef * ry * x1p / rx
    cx = cp * cxp - sp * cyp + (x1 + x2) / 2
    cy = sp * cxp + cp * cyp + (y1 + y2) / 2

    def angle(ux, uy, vx, vy):
        a = math.atan2(ux * vy - uy * vx, ux * vx + uy * vy)
        return a

    t1 = angle(1, 0, (x1p - cxp) / rx, (y1p - cyp) / ry)
    dt = angle((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry)
    if not sweep and dt > 0:
        dt -= 2 * math.pi
    elif sweep and dt < 0:
        dt += 2 * math.pi
    segments = max(1, math.ceil(abs(dt) / (math.pi / 2) - 1e-9))
    delta = dt / segments
    k = 4 / 3 * math.tan(delta / 4)
    out = []
    t = t1
    for _ in range(segments):
        c1, s1 = math.cos(t), math.sin(t)
        c2, s2 = math.cos(t + delta), math.sin(t + delta)
        p1 = (c1 - k * s1, s1 + k * c1)
        p2 = (c2 + k * s2, s2 - k * c2)
        p3 = (c2, s2)
        pts = []
        for px, py in (p1, p2, p3):
            px, py = px * rx, py * ry
            pts += [cp * px - sp * py + cx, sp * px + cp * py + cy]
        out.append(("C", *pts))
        t += delta
    return out


def parse_path(d):
    tokens = TOKEN.findall(d)
    i = 0
    cmd = None
    x = y = sx = sy = 0.0
    last_ctrl = None
    out = []

    def num():
        nonlocal i
        value = float(tokens[i])
        i += 1
        return value

    def flag():
        # Arc flags may be packed without separators, e.g. "011".
        nonlocal i
        token = tokens[i]
        if len(token) > 1 and token[0] in "01" and not token.startswith(("0.", "1.")):
            tokens[i] = token[1:]
            return int(token[0])
        i += 1
        return int(float(token))

    while i < len(tokens):
        if re.match(r"[A-Za-z]", tokens[i]):
            cmd = tokens[i]
            i += 1
            if cmd in "Zz":
                out.append(("Z",))
                x, y = sx, sy
                last_ctrl = None
                continue
        rel = cmd.islower()
        c = cmd.upper()
        ox, oy = (x, y) if rel else (0.0, 0.0)
        if c == "M":
            x, y = num() + ox, num() + oy
            sx, sy = x, y
            out.append(("M", x, y))
            cmd = "l" if rel else "L"
            last_ctrl = None
        elif c == "L":
            x, y = num() + ox, num() + oy
            out.append(("L", x, y))
            last_ctrl = None
        elif c == "H":
            x = num() + (x if rel else 0)
            out.append(("L", x, y))
            last_ctrl = None
        elif c == "V":
            y = num() + (y if rel else 0)
            out.append(("L", x, y))
            last_ctrl = None
        elif c == "C":
            x1, y1, x2, y2 = num() + ox, num() + oy, num() + ox, num() + oy
            x, y = num() + ox, num() + oy
            out.append(("C", x1, y1, x2, y2, x, y))
            last_ctrl = (x2, y2)
        elif c == "S":
            x1, y1 = (2 * x - last_ctrl[0], 2 * y - last_ctrl[1]) if last_ctrl else (x, y)
            x2, y2 = num() + ox, num() + oy
            x, y = num() + ox, num() + oy
            out.append(("C", x1, y1, x2, y2, x, y))
            last_ctrl = (x2, y2)
        elif c == "Q":
            qx, qy = num() + ox, num() + oy
            nx, ny = num() + ox, num() + oy
            out.append(("C", x + 2 / 3 * (qx - x), y + 2 / 3 * (qy - y),
                        nx + 2 / 3 * (qx - nx), ny + 2 / 3 * (qy - ny), nx, ny))
            x, y = nx, ny
            last_ctrl = None
        elif c == "A":
            rx, ry, phi = num(), num(), num()
            large, sweep = flag(), flag()
            nx, ny = num() + ox, num() + oy
            out += arc_to_cubics(x, y, rx, ry, phi, large, sweep, nx, ny)
            x, y = nx, ny
            last_ctrl = None
        else:
            raise ValueError(f"unsupported command {cmd}")
    return out


def rect(e):
    x, y = float(e.get("x", 0)), float(e.get("y", 0))
    w, h = float(e.get("width")), float(e.get("height"))
    rx = float(e.get("rx", e.get("ry", 0)))
    ry = float(e.get("ry", rx))
    rx, ry = min(rx, w / 2), min(ry, h / 2)
    if not rx:
        return [("M", x, y), ("L", x + w, y), ("L", x + w, y + h), ("L", x, y + h), ("Z",)]
    d = (f"M{x + rx} {y}H{x + w - rx}A{rx} {ry} 0 0 1 {x + w} {y + ry}V{y + h - ry}"
         f"A{rx} {ry} 0 0 1 {x + w - rx} {y + h}H{x + rx}A{rx} {ry} 0 0 1 {x} {y + h - ry}"
         f"V{y + ry}A{rx} {ry} 0 0 1 {x + rx} {y}Z")
    return parse_path(d)


def circle(e):
    cx, cy, r = float(e.get("cx")), float(e.get("cy")), float(e.get("r"))
    return parse_path(f"M{cx - r} {cy}A{r} {r} 0 1 1 {cx + r} {cy}A{r} {r} 0 1 1 {cx - r} {cy}Z")


def convert(path):
    root = ET.parse(path).getroot()
    segs = []
    for e in root.iter():
        tag = e.tag.split("}")[-1]
        if tag == "path":
            segs += parse_path(e.get("d"))
        elif tag == "rect":
            segs += rect(e)
        elif tag == "circle":
            segs += circle(e)
        elif tag == "line":
            segs += [("M", float(e.get("x1")), float(e.get("y1"))),
                     ("L", float(e.get("x2")), float(e.get("y2")))]
        elif tag in ("polyline", "polygon"):
            nums = [float(n) for n in re.findall(r"[-+]?(?:\d*\.\d+|\d+\.?)", e.get("points"))]
            pts = list(zip(nums[::2], nums[1::2]))
            segs += [("M", *pts[0])] + [("L", *p) for p in pts[1:]]
            if tag == "polygon":
                segs.append(("Z",))
    return segs


def fmt(v):
    text = f"{round(v, 3):.3f}".rstrip("0")
    return text + "0" if text.endswith(".") else text


def rust(name, segs):
    parts = []
    for s in segs:
        if s[0] == "Z":
            parts.append("Z")
        else:
            parts.append(f"{s[0]}({', '.join(fmt(v) for v in s[1:])})")
    body = ",\n    ".join(parts)
    return f"pub const {name}: &[Seg] = &[\n    {body},\n];\n"


if __name__ == "__main__":
    folder, version = sys.argv[1], sys.argv[2]
    names = sys.argv[3:]
    print(f"// Generated from Lucide v{version} (ISC, see licenses/lucide.txt).")
    print("// 24x24 grid, drawn with 2-unit round strokes. Do not edit by hand.\n")
    print("use super::Seg::{self, *};\n")
    for name in names:
        const = name.upper().replace("-", "_")
        print(rust(const, convert(f"{folder}/{name}.svg")))
