"""Dense size variants for table training: 24..1024 px and native (<= 4 MP),
RGB and gray, of each selection row's crop.

usage: python3 -I make_dense.py <selection.tsv> <src_root> <out_root>

Sources are fetched from the public imazen-26 R2 bucket into
<src_root>/<class>/ when missing. Crop rule from imazen-26
scripts/make_variant_set.py (crop_rect). Lanczos downscale only (no
upscaling). Needs ImageMagick (convert, identify) and curl.
Output: <out_root>/{rgb,gray}/{24,...,1024,native}/<class>/<id>_<crop>.png
"""
import csv, os, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

SIZES = [24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 512, 768, 1024]
BASE = 'https://codec-corpus.r2.imazen.org/imazen-26-png-v3/'
sel, src_root, out_root = sys.argv[1], sys.argv[2], sys.argv[3]
rows = list(csv.DictReader(open(sel), delimiter='\t'))


def crop_rect(w, h, label):
    if label == 'full':
        return (0, 0, w, h)
    kind, anchor = label.split('_', 1) if '_' in label else (label, 'center')
    frac = 0.5 if kind == 'c50' else 0.25
    side = max(1, int(min(w, h) * frac))
    pos = {'center': ((w - side) // 2, (h - side) // 2), 'tl': (0, 0), 'tr': (w - side, 0),
           'bl': (0, h - side), 'br': (w - side, h - side)}
    x, y = pos[anchor]
    return (x, y, side, side)


def run(r):
    src = os.path.join(src_root, r['class'], os.path.basename(r['path']))
    if not os.path.exists(src):
        os.makedirs(os.path.dirname(src), exist_ok=True)
        subprocess.run(['curl', '-sSf', '--retry', '3', '-o', src, BASE + r['path']], check=True)
    out = subprocess.run(['identify', '-format', '%w %h %[channels]\n', src], capture_output=True, text=True).stdout
    w, h, ch = out.split('\n')[0].split()[:3]
    w, h = int(w), int(h)
    x, y, cw, chh = crop_rect(w, h, r['crop'])
    base = f"{r['id']}_{r['crop']}"
    fmts = ['rgb', 'gray'] if ch.startswith('srgb') else ['rgb']
    for fmt in fmts:
        ct = '0' if (fmt == 'gray' or ch.startswith('gray')) else ('6' if ch == 'srgba' else '2')
        cmd = ['nice', '-n', '19', 'convert', src, '-crop', f'{cw}x{chh}+{x}+{y}', '+repage']
        if fmt == 'gray':
            cmd += ['-colorspace', 'Gray']
        cmd += ['-depth', '8', '-define', f'png:color-type={ct}', '-define', 'png:compression-level=1']
        # native: center crop to <= 4 MP, no resampling
        sw, sh = cw, chh
        while sw * sh > 4_000_000:
            sw, sh = int(sw * 0.95), int(sh * 0.95)
        nd = os.path.join(out_root, fmt, 'native', r['class'])
        os.makedirs(nd, exist_ok=True)
        cmd += ['(', '+clone', '-crop', f'{sw}x{sh}+{(cw - sw) // 2}+{(chh - sh) // 2}', '+repage',
                '-write', f'PNG:{nd}/{base}.png', '+delete', ')']
        for s in SIZES:
            if s > min(cw, chh):
                continue
            d = os.path.join(out_root, fmt, str(s), r['class'])
            os.makedirs(d, exist_ok=True)
            cmd += ['(', '+clone', '-filter', 'Lanczos', '-resize', f'{s}x{s}!', '-write', f'PNG:{d}/{base}.png',
                    '+delete', ')']
        cmd += ['null:']
        subprocess.run(cmd, check=True)
    return r['id']


with ThreadPoolExecutor(6) as ex:
    done = list(ex.map(run, rows))
print(len(done), 'units')
