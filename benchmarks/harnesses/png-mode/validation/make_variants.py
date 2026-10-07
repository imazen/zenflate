"""Build size variants of the held-out K300 representative crops.

Crop rule copied from imazen-26 `scripts/make_variant_set.py` (`crop_rect`).
Sources are fetched from the public imazen-26 R2 bucket into
$LADDER_ROOT/img/<class>/ when missing. Needs ImageMagick (`convert`,
`identify`) and curl.
"""
import csv, os, subprocess, sys
from concurrent.futures import ThreadPoolExecutor
root = os.environ.get('LADDER_ROOT', os.path.expanduser('~/tmp/ladder-corpus'))
rows = list(csv.DictReader(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'selection.tsv')), delimiter='\t'))

def crop_rect(w, h, label):
    if label == 'full':
        return None
    kind, anchor = label.split('_', 1) if '_' in label else (label, 'center')
    frac = 0.5 if kind == 'c50' else 0.25
    side = max(1, int(min(w, h) * frac))
    pos = {'center': ((w - side) // 2, (h - side) // 2), 'tl': (0, 0), 'tr': (w - side, 0),
           'bl': (0, h - side), 'br': (w - side, h - side)}
    x, y = pos[anchor]
    return (x, y, side, side)

def info(path):
    out = subprocess.run(['identify', '-format', '%w %h %[channels]\n', path], capture_output=True, text=True).stdout.split('\n')[0].split()
    return int(out[0]), int(out[1]), out[2]

CT = {'gray': '0', 'graya': '4', 'srgb': '2', 'srgba': '6', 'rgb': '2', 'rgba': '6'}

BASE = 'https://codec-corpus.r2.imazen.org/imazen-26-png-v3/'

def run(r):
    src = f"{root}/img/{r['class']}/{os.path.basename(r['path'])}"
    if not os.path.exists(src):
        os.makedirs(os.path.dirname(src), exist_ok=True)
        subprocess.run(['curl', '-sSf', '--retry', '3', '-o', src, BASE + r['path']], check=True)
    w, h, ch = info(src)
    ct = CT.get(ch, '2')
    rect = crop_rect(w, h, r['crop'])
    if rect is None:
        rect = (0, 0, w, h)
    x, y, cw, chh = rect
    base = f"{r['id']}_{r['crop']}"
    common = ['-depth', '8', '-define', f'png:color-type={ct}', '-define', 'png:compression-level=1']
    made = []
    # native: crop region, then center crop to <= 4 MP (no resampling)
    side_w, side_h = cw, chh
    while side_w * side_h > 4_000_000:
        side_w, side_h = int(side_w * 0.95), int(side_h * 0.95)
    nx, ny = x + (cw - side_w) // 2, y + (chh - side_h) // 2
    jobs = [('native', ['-crop', f'{side_w}x{side_h}+{nx}+{ny}', '+repage'])]
    for name, target in [('1mp', 1000), ('256', 256), ('64', 64)]:
        if min(cw, chh) >= target:  # never upscale
            jobs.append((name, ['-crop', f'{cw}x{chh}+{x}+{y}', '+repage', '-filter', 'Lanczos',
                                '-resize', f'{target}x{target}!']))
    for name, ops in jobs:
        d = f"{root}/sizes/{name}/{r['class']}"
        os.makedirs(d, exist_ok=True)
        out = f'{d}/{base}.png'
        if not os.path.exists(out):
            subprocess.run(['nice', '-n', '19', 'convert', src, *ops, *common, f'PNG:{out}'], check=True)
        made.append(name)
    return base, made

with ThreadPoolExecutor(6) as ex:
    for base, made in ex.map(run, rows):
        pass
for name in ['native', '1mp', '256', '64']:
    n = sum(len(fs) for _, _, fs in os.walk(f'{root}/sizes/{name}'))
    print(name, n)
