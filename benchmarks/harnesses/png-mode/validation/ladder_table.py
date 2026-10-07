"""One ladder, every bucket x transform: ratio, speed, paired step stats.

usage: python3 -I ladder_table.py <prefix> codec1,codec2,...   (logs: <prefix>-{64,256,native}.log)
"""
import collections, math, sys

prefix, ladder = sys.argv[1], sys.argv[2].split(',')
buckets = ['native', '256', '64']
transforms = ['png_adaptive', 'png_none']


def load(path, transform):
    size = collections.defaultdict(dict)
    secs = collections.defaultdict(dict)
    inb = {}
    for line in open(path):
        if line.startswith('SIZE,') or line.startswith('TIME,'):
            p = line.rstrip('\n').split(',')
            if p[1] != transform:
                continue
            if p[0] == 'SIZE':
                size[p[3]][p[2]] = int(p[4])
            else:
                secs[p[3]][p[2]] = float(p[4])
                inb[p[2]] = int(p[5])
    return size, secs, inb


def mean_sd(xs):
    m = sum(xs) / len(xs)
    return m, math.sqrt(sum((x - m) ** 2 for x in xs) / (len(xs) - 1))


pct = lambda x: (math.exp(x) - 1) * 100
for t in transforms:
    print(f"\n### {t}")
    hdr = f"{'lvl':>3s} {'codec':20s}"
    for b in buckets:
        hdr += f" | {b+' ratio':>12s} {'MiB/s':>6s} {'step%':>13s} {'inv':>7s} {'worst%':>7s}"
    print(hdr)
    data = {b: load(f'{prefix}-{b}.log', t) for b in buckets}
    for i, c in enumerate(ladder):
        row = f"{i+1:3d} {c:20s}"
        for b in buckets:
            size, secs, inb = data[b]
            units = sorted(inb)
            n = len(units)
            tin = sum(inb[u] for u in units)
            ratio = tin / sum(size[c][u] for u in units)
            speed = tin / sum(secs[c][u] for u in units) / 2**20
            if i == 0:
                row += f" | {ratio:12.4f} {speed:6.0f} {'':>13s} {'':>7s} {'':>7s}"
            else:
                p = ladder[i - 1]
                d = [math.log(size[c][u] / size[p][u]) for u in units]
                m, sd = mean_sd(d)
                ci = 1.96 * sd / math.sqrt(n)
                inv = sum(1 for x in d if x > 0)
                row += f" | {ratio:12.4f} {speed:6.0f} {pct(m):+6.2f}+-{pct(ci):4.2f} {inv:3d}/{n:<3d} {pct(max(d)):+7.2f}"
        print(row)
