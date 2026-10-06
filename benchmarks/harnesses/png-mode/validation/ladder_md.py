"""Markdown tables for a ladder from pngmode-bench PER_IMAGE logs.

usage: python3 -I ladder_md.py <prefix> <codec,...>   (logs: <prefix>-{native,256,64}.log)
"""
import collections, math, sys

prefix, ladder = sys.argv[1], sys.argv[2].split(',')
buckets = [('native', 'native sizes'), ('256', '256x256'), ('64', '64x64')]
transforms = [('png_adaptive', 'adaptive filter'), ('png_none', 'filter None (unfiltered)')]


def load(path, transform):
    size = collections.defaultdict(dict)
    secs = collections.defaultdict(dict)
    inb = {}
    for line in open(path):
        if line.startswith(('SIZE,', 'TIME,')):
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


def name(c):
    if c.startswith('png-e'):
        return f"`png({int(c[5:])})`"
    if c.startswith('zf-e'):
        return f"`new({c[4:]})`"
    return c


pct = lambda x: (math.exp(x) - 1) * 100
for t, tlabel in transforms:
    for b, blabel in buckets:
        size, secs, inb = load(f'{prefix}-{b}.log', t)
        units = sorted(inb)
        n = len(units)
        tin = sum(inb[u] for u in units)
        print(f"\n#### {blabel}, {tlabel} ({n} images)\n")
        print("| Level | Ratio | MiB/s | Step: size change, mean ± 95% CI | SD | Images worse | Worst |")
        print("|---|---|---|---|---|---|---|")
        for i, c in enumerate(ladder):
            ratio = tin / sum(size[c][u] for u in units)
            speed = tin / sum(secs[c][u] for u in units) / 2**20
            if i == 0:
                print(f"| {name(c)} | {ratio:.3f} | {speed:.0f} | | | | |")
                continue
            p = ladder[i - 1]
            d = [math.log(size[c][u] / size[p][u]) for u in units]
            m, sd = mean_sd(d)
            ci = 1.96 * sd / math.sqrt(n)
            inv = sum(1 for x in d if x > 0)
            print(f"| {name(c)} | {ratio:.3f} | {speed:.0f} | {pct(m):+.2f}% ± {pct(ci):.2f} | {sd*100:.2f}% | {inv} | {pct(max(d)):+.2f}% |")
