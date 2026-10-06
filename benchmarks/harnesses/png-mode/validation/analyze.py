"""Ladder validation statistics from pngmode-bench PER_IMAGE logs.

usage: python3 -I analyze.py <bucket.log> [--selection selection.tsv] [--transform png_adaptive]
                             [--pool PREFIXES] [--levels N] [--ladder codec,codec,...]

Prints per-codec aggregates (ratio, cluster-weighted ratio, per-unit log-ratio
mean/SD, aggregate and per-unit speed) and, for a ladder, the paired step
statistics: mean +- SD of per-unit log size change, 95% CI, inversion rate,
worst inversion, paired speed ratio.
"""
import argparse, collections, csv, math, os, sys

ap = argparse.ArgumentParser()
ap.add_argument('log')
ap.add_argument('--selection', default=os.path.expanduser('~/tmp/ladder-corpus/selection.tsv'))
ap.add_argument('--transform', default='png_adaptive')
ap.add_argument('--pool', default='ultra-,v-')
ap.add_argument('--levels', type=int, default=0)
ap.add_argument('--ladder', default='')
ap.add_argument('--show', default='')
args = ap.parse_args()

# unit -> cluster weight (unit file names are <id>_<crop>.png)
weight = {}
for r in csv.DictReader(open(args.selection), delimiter='\t'):
    weight[f"{r['id']}_{r['crop']}.png"] = float(r['cluster_size'])

size = collections.defaultdict(dict)   # codec -> unit -> bytes
secs = collections.defaultdict(dict)   # codec -> unit -> seconds
inb = {}                                # unit -> input bytes
for line in open(args.log):
    if line.startswith('SIZE,'):
        _, t, unit, codec, n = line.rstrip('\n').split(',')
        if t == args.transform:
            size[codec][unit] = int(n)
    elif line.startswith('TIME,'):
        _, t, unit, codec, s, n = line.rstrip('\n').split(',')
        if t == args.transform:
            secs[codec][unit] = float(s)
            inb[unit] = int(n)

units = sorted(inb)
n = len(units)

def mean_sd(xs):
    m = sum(xs) / len(xs)
    v = sum((x - m) ** 2 for x in xs) / (len(xs) - 1) if len(xs) > 1 else 0.0
    return m, math.sqrt(v)

def agg(codec):
    s = size[codec]; t = secs[codec]
    tin = sum(inb[u] for u in units)
    ratio = tin / sum(s[u] for u in units)
    wr = sum(weight.get(u, 1) * inb[u] for u in units) / sum(weight.get(u, 1) * s[u] for u in units)
    lr_m, lr_sd = mean_sd([math.log(inb[u] / s[u]) for u in units])
    speed = tin / sum(t[u] for u in units) / 2**20
    ls_m, ls_sd = mean_sd([math.log(inb[u] / t[u] / 2**20) for u in units])
    return dict(ratio=ratio, wratio=wr, lr_m=lr_m, lr_sd=lr_sd, speed=speed, uspeed=math.exp(ls_m), ls_sd=ls_sd)

def paired(a, b):
    """Step a -> b: per-unit log(size_b / size_a), negative = b smaller."""
    d = [math.log(size[b][u] / size[a][u]) for u in units]
    m, sd = mean_sd(d)
    ci = 1.96 * sd / math.sqrt(len(d))
    inv = [x for x in d if x > 0]
    sp = [math.log(secs[b][u] / secs[a][u]) for u in units]
    spm, spsd = mean_sd(sp)
    return dict(m=m, sd=sd, ci=ci, inv=len(inv), worst=max(d), slow=math.exp(spm), slow_sd=spsd)

A = {c: agg(c) for c in size if len(size[c]) == n}
print(f"# {args.log}  transform={args.transform}  units={n}  input={sum(inb.values())/2**20:.1f} MiB")
print(f"{'codec':18s} {'ratio':>7s} {'wratio':>7s} {'lnR mean':>9s} {'lnR sd':>7s} {'MiB/s':>8s} {'unitMiB/s':>9s} {'ln sd':>6s}")
for c in sorted(A, key=lambda c: -A[c]['speed']):
    if args.show and not any(c.startswith(p) for p in args.show.split(',')):
        continue
    a = A[c]
    print(f"{c:18s} {a['ratio']:7.4f} {a['wratio']:7.4f} {a['lr_m']:9.4f} {a['lr_sd']:7.4f} {a['speed']:8.0f} {a['uspeed']:9.0f} {a['ls_sd']:6.3f}")

def show_ladder(lad):
    print(f"\n## ladder ({len(lad)} levels): step stats are per-unit paired, 95% CI of the mean")
    print(f"{'lvl':>3s} {'codec':18s} {'ratio':>7s} {'MiB/s':>7s} {'step size %':>14s} {'sd %':>6s} {'inv':>7s} {'worst %':>8s} {'slower x':>8s}")
    prev = None
    for i, c in enumerate(lad, 1):
        a = A[c]
        if prev is None:
            print(f"{i:3d} {c:18s} {a['ratio']:7.4f} {a['speed']:7.0f}")
        else:
            p = paired(prev, c)
            pct = lambda x: (math.exp(x) - 1) * 100
            print(f"{i:3d} {c:18s} {a['ratio']:7.4f} {a['speed']:7.0f} {pct(p['m']):+7.2f}+-{pct(p['ci']):4.2f} {p['sd']*100:6.2f} {p['inv']:3d}/{n:<3d} {pct(p['worst']):+8.2f} {p['slow']:8.2f}")
        prev = c

if args.ladder:
    show_ladder(args.ladder.split(','))

if args.levels:
    pool = [c for c in A if any(c.startswith(p) for p in args.pool.split(','))]
    # Pareto front on (aggregate speed, aggregate ratio)
    front = []
    for c in sorted(pool, key=lambda c: -A[c]['speed']):
        if not front or A[c]['ratio'] > A[front[-1]]['ratio']:
            front.append(c)
    print("\n## Pareto front (aggregate):", ' '.join(f"{c}({A[c]['ratio']:.3f}@{A[c]['speed']:.0f})" for c in front))
    # Greedy: walk the front, keep a level only if its paired gain over the
    # previous kept level is significant (CI excludes 0), then thin to N levels
    # at even log-speed spacing.
    kept = [front[0]]
    for c in front[1:]:
        p = paired(kept[-1], c)
        if p['m'] + p['ci'] < 0:
            kept.append(c)
    print("## significant steps:", ' '.join(kept))
    if len(kept) > args.levels:
        ls = [math.log(A[c]['speed']) for c in kept]
        targets = [ls[0] + (ls[-1] - ls[0]) * i / (args.levels - 1) for i in range(args.levels)]
        chosen = []
        for t in targets:
            c = min(kept, key=lambda c: abs(math.log(A[c]['speed']) - t))
            if c not in chosen:
                chosen.append(c)
        kept = sorted(chosen, key=lambda c: -A[c]['speed'])
    show_ladder(kept)
