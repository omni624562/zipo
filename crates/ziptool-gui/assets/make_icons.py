"""產生 Zipo 圖示：程式圖示（ziptool.ico）與壓縮檔圖示（archive.ico），以 SDF 繪製並抗鋸齒。"""
import math
import os
import struct
import sys
import zlib

OUT = sys.argv[1]
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def hexc(h, a=1.0):
    h = h.lstrip('#')
    return (int(h[0:2], 16) / 255, int(h[2:4], 16) / 255, int(h[4:6], 16) / 255, a)


def lerp(c1, c2, t):
    return tuple(a + (b - a) * t for a, b in zip(c1, c2))


def sd_rrect(px, py, x0, y0, x1, y1, r):
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    hx, hy = (x1 - x0) / 2, (y1 - y0) / 2
    r = min(r, hx, hy)
    qx, qy = abs(px - cx) - hx + r, abs(py - cy) - hy + r
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def sd_half(px, py, ax, ay, nx, ny):
    """半平面：n 指向外側"""
    l = math.hypot(nx, ny)
    return ((px - ax) * nx + (py - ay) * ny) / l


class Canvas:
    def __init__(self, n):
        self.n = n
        self.s = n / 256  # 設計座標 256 → 像素
        self.px = [[(0.0, 0.0, 0.0, 0.0)] * n for _ in range(n)]

    def draw(self, sdf, color, bbox=None, soft=0.0):
        """sdf(x, y) 以設計座標回傳距離；color 為顏色或 (x, y) -> 顏色。"""
        n, s = self.n, self.s
        x0, y0, x1, y1 = bbox if bbox else (0, 0, 256, 256)
        pad = soft + 2 / s
        ix0, iy0 = max(0, int((x0 - pad) * s)), max(0, int((y0 - pad) * s))
        ix1, iy1 = min(n, int((x1 + pad) * s) + 1), min(n, int((y1 + pad) * s) + 1)
        for j in range(iy0, iy1):
            for i in range(ix0, ix1):
                x, y = (i + 0.5) / s, (j + 0.5) / s
                d = sdf(x, y) * s  # 像素距離
                if soft:
                    cov = max(0.0, min(1.0, 0.5 - d / (soft * s)))
                else:
                    cov = max(0.0, min(1.0, 0.5 - d))
                if cov <= 0:
                    continue
                c = color(x, y) if callable(color) else color
                a = c[3] * cov
                r, g, b, da = self.px[j][i]
                oa = a + da * (1 - a)
                if oa > 0:
                    self.px[j][i] = (
                        (c[0] * a + r * da * (1 - a)) / oa,
                        (c[1] * a + g * da * (1 - a)) / oa,
                        (c[2] * a + b * da * (1 - a)) / oa,
                        oa,
                    )

    def png(self):
        raw = b''
        for row in self.px:
            raw += b'\0' + bytes(
                v for p in row for v in (round(p[0] * 255), round(p[1] * 255), round(p[2] * 255), round(p[3] * 255))
            )

        def chunk(t, d):
            return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xFFFFFFFF)

        return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', self.n, self.n, 8, 6, 0, 0, 0))
                + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


def rect_px(c, x0, y0, x1, y1, color, r=0.0):
    """以像素座標繪製（小尺寸用，邊緣對齊像素）"""
    s = c.s
    c.draw(lambda x, y: sd_rrect(x, y, x0 / s, y0 / s, x1 / s, y1 / s, r / s), color, (x0 / s, y0 / s, x1 / s, y1 / s))


def teeth(c, top, bottom, color):
    """沿 x=128 往下交錯的拉鍊齒；間距對齊像素格線。top/bottom 為設計座標。"""
    s = c.s
    step = max(2, round(8 * s))  # 像素
    th = step - 1 if step <= 3 else step * 0.7
    y = round(top * s)
    k = 0
    while y + th <= bottom * s:
        if k % 2 == 0:
            xa, xb = 116, 132
        else:
            xa, xb = 124, 140
        c.draw(lambda px, py, xa=xa, xb=xb, y=y: sd_rrect(px, py, xa, y / s, xb, (y + th) / s, min(2, th / s / 3)),
               color, (xa, y / s, xb, (y + th) / s))
        y += step
        k += 1


def pull(c, sy, color, hole):
    """拉頭＋拉片（設計座標）"""
    c.draw(lambda px, py: sd_rrect(px, py, 110, sy, 146, sy + 22, 6), color, (110, sy, 146, sy + 22))
    c.draw(lambda px, py: sd_rrect(px, py, 116, sy + 14, 140, sy + 66, 10), color, (116, sy + 14, 140, sy + 66))
    c.draw(lambda px, py: sd_rrect(px, py, 122, sy + 40, 134, sy + 58, 5), hole, (122, sy + 40, 134, sy + 58))


def small_zipper(c, n, top, color, pull_color=None):
    """小尺寸拉鍊：單像素交錯的齒，加上拉頭與拉片"""
    cx = n // 2
    ze = round(n * 0.6)
    for j in range(top, ze):
        col = cx - 1 if j % 2 == 0 else cx
        rect_px(c, col, j, col + 1, j + 1, color)
    pc = pull_color or color
    rect_px(c, cx - 2, ze, cx + 2, ze + 2, pc, 0.5)
    rect_px(c, cx - 1, ze + 1, cx + 1, ze + 1 + max(3, round(n * 0.2)), pc, 0.5)


def app_icon(n):
    c = Canvas(n)
    top_c, bot_c = hexc('#3AA0F0'), hexc('#0A5FB4')
    lid_t, lid_b = hexc('#7CC4FA'), hexc('#3D9BEA')
    white = hexc('#FFFFFF', 0.97)
    grad = lambda y0, y1, a, b: (lambda x, y: lerp(a, b, (y - y0) / (y1 - y0)))
    if n < 32:
        s = c.s
        lb = round(n * 0.375)
        rect_px(c, 1, round(n * 0.3), n - 1, n - 0.5, grad(0, 256, top_c, bot_c), 2)
        rect_px(c, 0, 1, n, lb, grad(1 / s, lb / s, lid_t, lid_b), 1.5)
        small_zipper(c, n, 1, white)
        return c
    if n >= 48:
        c.draw(lambda x, y: sd_rrect(x, y, 22, 40, 234, 246, 32), (0, 0, 0, 0.28), soft=10)
    c.draw(lambda x, y: sd_rrect(x, y, 18, 44, 238, 242, 30), grad(44, 242, top_c, bot_c))
    c.draw(lambda x, y: sd_rrect(x, y, 12, 22, 244, 96, 22), grad(22, 96, lid_t, lid_b))
    # 蓋子下緣的陰影線
    c.draw(lambda x, y: sd_rrect(x, y, 18, 96, 238, 102, 0), (0, 0, 0, 0.12), (18, 96, 238, 102))
    teeth(c, 30, 150, white)
    pull(c, 146, white, grad(44, 242, top_c, bot_c))
    return c


def file_icon(n):
    c = Canvas(n)
    s = c.s
    blue_t, blue_b = hexc('#3AA0F0'), hexc('#0A5FB4')
    border, white, fold_c = hexc('#8A8A8A'), hexc('#FFFFFF'), hexc('#DCEBFA')
    grad = lambda x, y: lerp(blue_t, blue_b, y / 256)
    if n < 32:
        x0, x1 = 2, n - 2
        f = round(n * 0.3)
        page = lambda x, y: max(sd_rrect(x, y, x0 / s, 0, x1 / s, n / s, 0.5 / s), sd_half(x, y, (x1 - f) / s, 0, 1, -1))
        c.draw(page, border)
        c.draw(lambda x, y: page(x, y) + 1 / s, white)
        cx = n // 2
        ze = round(n * 0.6)
        rect_px(c, cx - 2, 1, cx + 2, ze, grad)
        small_zipper(c, n, 1, white, blue_b)
        corner = lambda x, y: max(sd_half(x, y, (x1 - f) / s, 0, -1, 1), x - x1 / s, -y, (x1 - f) / s - x, y - f / s)
        c.draw(corner, border)
        c.draw(lambda x, y: corner(x, y) + 1 / s, fold_c)
        return c
    x0, y0, x1, y1, fold = 40, 8, 216, 248, 56
    px1 = max(2.5, 1 / s)

    def page(x, y):
        return max(sd_rrect(x, y, x0, y0, x1, y1, 8), sd_half(x, y, x1 - fold, y0, 1, -1))

    if n >= 48:
        c.draw(lambda x, y: page(x, y - 3), (0, 0, 0, 0.22), soft=8)
    c.draw(page, border)
    c.draw(lambda x, y: page(x, y) + px1, white)
    c.draw(lambda x, y: sd_rrect(x, y, 104, y0 + px1, 152, 176, 0), grad, (104, y0, 152, 176))
    teeth(c, y0 + px1 + 4, 166, white)
    pull(c, 168, blue_b, white)

    def corner(x, y):
        return max(sd_half(x, y, x1 - fold, y0, -1, 1), x - x1, y0 - y, (x1 - fold) - x, y - (y0 + fold))
    c.draw(corner, border)
    c.draw(lambda x, y: corner(x, y) + px1, fold_c)
    return c


def write_ico(path, canvases):
    pngs = [c.png() for c in canvases]
    head = struct.pack('<HHH', 0, 1, len(pngs))
    off = 6 + 16 * len(pngs)
    dirs = b''
    for c, p in zip(canvases, pngs):
        sz = c.n if c.n < 256 else 0
        dirs += struct.pack('<BBBBHHII', sz, sz, 0, 0, 1, 32, len(p), off)
        off += len(p)
    open(path, 'wb').write(head + dirs + b''.join(pngs))


def preview(path, sets, bg):
    """每列一個圖示，依序排出各尺寸（原尺寸 + 放大 4 倍的 16/24/32），方便檢查。"""
    W, H = 1300, 300 * len(sets)
    img = [[bg] * W for _ in range(H)]
    for r, cs in enumerate(sets):
        x = 10
        for c in cs:
            for scale in ([1] if c.n >= 48 else [1, 4]):
                for j in range(c.n * scale):
                    for i in range(c.n * scale):
                        p = c.px[j // scale][i // scale]
                        yy, xx = 10 + r * 300 + j, x + i
                        if yy < H and xx < W:
                            b = img[yy][xx]
                            img[yy][xx] = tuple(p[k] * p[3] + b[k] * (1 - p[3]) for k in range(3))
                x += c.n * scale + 10
    cv = Canvas(1)
    cv.n = None
    raw = b''.join(b'\0' + bytes(round(v * 255) for p in row for v in p) for row in img)

    def chunk(t, d):
        return struct.pack('>I', len(d)) + t + d + struct.pack('>I', zlib.crc32(t + d) & 0xFFFFFFFF)

    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', W, H, 8, 2, 0, 0, 0))
                           + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


apps = [app_icon(n) for n in SIZES]
files = [file_icon(n) for n in SIZES]
write_ico(os.path.join(OUT, 'ziptool.ico'), apps)
write_ico(os.path.join(OUT, 'archive.ico'), files)
pv = os.environ.get('PREVIEW')
if pv:
    preview(pv + '_light.png', [apps, files], (1, 1, 1))
    preview(pv + '_dark.png', [apps, files], (0.12, 0.12, 0.12))
print('ok')

# 視窗與工作列圖示（由程式在執行時載入）
open(os.path.join(OUT, 'ziptool-256.png'), 'wb').write(apps[-1].png())
