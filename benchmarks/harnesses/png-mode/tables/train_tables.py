"""Model fixed and per-image Huffman tables for png(1) on held-out streams.

usage: train_tables.py hist_train.tsv hist_holdout.tsv [--out tables.json]
needs: numpy

Rows come from the harness's token_hist binary: per filtered image, the
286-symbol token histograms of the whole stream, its first 2 KiB and first
8 KiB. Per stream, cost = blocks * (3 + header bits) + code bits + distance
bits (1 per match for dynamic tables, 5 for the static code); length extra
bits are the same for every table and left out. Tables are trained on the
train streams under --small bytes and evaluated on the held-out streams.
This is a model (see deflate_cost.py), not encoder output.
"""
import argparse, collections, json, math, os, random, sys
import numpy as np
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import deflate_cost as dc

ap = argparse.ArgumentParser()
ap.add_argument('train')
ap.add_argument('holdout')
ap.add_argument('--out', default='tables.json')
ap.add_argument('--maxlen', type=int, default=12)
ap.add_argument('--small', type=int, default=64 * 1024, help='fixed-table size limit (bytes)')
args = ap.parse_args()
NSYM = 286

# fdeflate's fixed literal/length lengths (fdeflate a713d02, MIT OR Apache-2.0).
FDEFLATE = (
    [2, 3, 4, 5, 5, 6, 6, 7, 7, 7] + [8] * 5 + [9] * 7 + [10] * 9 + [11] * 12 + [12] * 171 +
    [11] * 10 + [10, 11] + [10] * 9 + [9] * 5 + [8, 9] + [8] * 5 +
    [7, 7, 7, 6, 6, 6, 5, 4, 3, 12, 12, 12, 9, 9, 11, 10, 11, 11, 10] + [11] * 6 + [12, 11] +
    [12] * 11 + [9]
)
assert len(FDEFLATE) == NSYM and abs(sum(2.0 ** -l for l in FDEFLATE) - 1) < 1e-12


def load(path):
    meta, H, H2, H8 = [], [], [], []
    for line in open(path):
        p = line.rstrip('\n').split('\t')
        rel, ch, nbytes = p[0], int(p[1]), int(p[4])
        v = list(map(int, p[5:]))
        parts = rel.split('/')
        meta.append(dict(rel=rel, size=parts[1], ch=ch, n=nbytes))
        H.append(v[0:NSYM]); H2.append(v[NSYM:2 * NSYM]); H8.append(v[2 * NSYM:3 * NSYM])
    f = lambda x: np.array(x, dtype=np.float64)
    return meta, f(H), f(H2), f(H8)


Mt, Ht, H2t, H8t = load(args.train)
Mh, Hh, H2h, H8h = load(args.holdout)
nt = np.array([m['n'] for m in Mt]); nh = np.array([m['n'] for m in Mh])
print(f"train {len(Mt)} streams ({(nt < args.small).sum()} small), holdout {len(Mh)} streams ({(nh < args.small).sum()} small)")
blk_h = dc.nblocks(nh)
matches_h = Hh[:, 257:].sum(axis=1)
Nt = Ht / np.maximum(Ht.sum(axis=1, keepdims=True), 1)  # each stream weighs the same
small_t = nt < args.small


def bits_one(H, L, blocks):
    return blocks * (3 + dc.header_bits(L)) + H @ L + H[:, 257:].sum(axis=1)


def bits_each(H, tables, blocks):
    return np.array([blocks[i] * (3 + dc.header_bits(L)) + H[i] @ L + H[i, 257:].sum() for i, L in enumerate(tables)])


def bits_pick(H, tables, pick, blocks):
    hdr = np.array([dc.header_bits(L) for L in tables])
    data = np.stack([H @ L for L in tables])
    return blocks * (3 + hdr[pick]) + data[pick, np.arange(H.shape[0])] + H[:, 257:].sum(axis=1)


results = {}
results['fdeflate fixed'] = bits_one(Hh, np.array(FDEFLATE, dtype=np.float64), blk_h)
results['static (BTYPE=01)'] = blk_h * 3 + Hh @ dc.STATIC + 5 * matches_h
print("per-stream tables (holdout)...", flush=True)
results['per-stream exact'] = bits_each(Hh, [dc.build(h, args.maxlen) for h in Hh], blk_h)
for name, Hs in [('first-8K', H8h), ('first-2K', H2h)]:
    results[f'table from {name} sample'] = bits_each(Hh, [dc.build(h + 1, args.maxlen) for h in Hs], blk_h)

L_glob = dc.build(Nt.mean(axis=0), args.maxlen)
L_glob_small = dc.build(Nt[small_t].mean(axis=0), args.maxlen)
results['global (all train)'] = bits_one(Hh, L_glob, blk_h)
results['global (small train)'] = bits_one(Hh, L_glob_small, blk_h)
chs = sorted({m['ch'] for m in Mt})
L_ch = {c: dc.build(Nt[np.array([m['ch'] == c for m in Mt]) & small_t].mean(axis=0), args.maxlen) for c in chs}
results['per-channel-count (small train)'] = bits_each(Hh, [L_ch[m['ch']] for m in Mh], blk_h)


def lloyd(N, K, iters=15, seed=1):
    """K tables by Lloyd iterations on coded bits per symbol, k-means++-style seeding."""
    rng = random.Random(seed)
    mean = N.mean(axis=0)
    tables = [dc.build(mean, args.maxlen)]
    while len(tables) < K:
        cur = np.min(np.stack([N @ L for L in tables]), axis=0)
        w = cur - cur.min() + 1e-12
        j = int(np.searchsorted(np.cumsum(w), rng.random() * w.sum()))
        tables.append(dc.build(N[j] + 0.05 * mean, args.maxlen))
    for _ in range(iters):
        a = np.stack([N @ L for L in tables]).argmin(axis=0)
        tables = [dc.build(N[a == k].mean(axis=0), args.maxlen) if (a == k).any() else tables[k] for k in range(K)]
    return tables


codebooks = {}
for K in [2, 4, 8, 16]:
    print(f"codebook K={K}...", flush=True)
    tabs = lloyd(Nt[small_t], K)
    codebooks[K] = tabs
    hdr = np.array([dc.header_bits(L) for L in tabs])
    oracle = (np.stack([Hh @ L for L in tabs]) + blk_h * hdr[:, None]).argmin(axis=0)
    results[f'codebook K={K} oracle'] = bits_pick(Hh, tabs, oracle, blk_h)
    for name, Hs in [('8K', H8h), ('2K', H2h)]:
        pick = (np.stack([Hs @ L for L in tabs]) + hdr[:, None]).argmin(axis=0)
        results[f'codebook K={K} pick@{name}'] = bits_pick(Hh, tabs, pick, blk_h)

base = results['fdeflate fixed']
own = results['per-stream exact']


def report(title, idx):
    if len(idx) < 2:
        return
    print(f"\n## {title} (n={len(idx)})")
    print(f"{'method':34s} {'KiB':>10s} {'vs fdeflate':>12s} {'sd':>6s} {'vs exact':>9s} {'sd':>6s} {'worse-than-fd':>13s}")
    for k, v in results.items():
        d_fd = np.log(v[idx] / base[idx]); d_own = np.log(v[idx] / own[idx])
        print(f"{k:34s} {v[idx].sum()/8/1024:10.1f} {math.expm1(d_fd.mean())*100:+11.2f}% {d_fd.std(ddof=1)*100:6.2f} {math.expm1(d_own.mean())*100:+8.2f}% {d_own.std(ddof=1)*100:6.2f} {int((d_fd > 0).sum()):6d}")


allidx = np.arange(len(Mh))
chh = np.array([m['ch'] for m in Mh])
report('holdout, streams < small limit (fixed table applies)', allidx[nh < args.small])
report('holdout, streams >= small limit (sampled table today)', allidx[nh >= args.small])
for c in chs:
    report(f'holdout small, {c} channel(s)', allidx[(nh < args.small) & (chh == c)])
sizes = collections.defaultdict(list)
for i, m in enumerate(Mh):
    if m['n'] < args.small:
        sizes[m['size']].append(i)
print("\n## small holdout by target size: mean change vs fdeflate fixed")
keys = ['static (BTYPE=01)', 'per-stream exact', 'global (small train)', 'per-channel-count (small train)',
        'codebook K=4 pick@2K', 'codebook K=8 pick@2K', 'codebook K=16 pick@2K']
print(f"{'size':>6s} {'n':>4s} " + ' '.join(f'{k[:14]:>14s}' for k in keys))
for s in sorted(sizes, key=lambda x: (x == 'native', int(x) if x.isdigit() else 0)):
    ii = sizes[s]
    print(f"{s:>6s} {len(ii):4d} " + ' '.join(f"{math.expm1(np.log(results[k][ii] / base[ii]).mean())*100:+13.2f}%" for k in keys))

json.dump({'fdeflate': FDEFLATE, 'global_all': [int(x) for x in L_glob], 'global_small': [int(x) for x in L_glob_small],
           'per_ch': {str(c): [int(x) for x in L] for c, L in L_ch.items()},
           'codebooks': {str(K): [[int(x) for x in L] for L in t] for K, t in codebooks.items()}}, open(args.out, 'w'))
print(f"\nwrote {args.out}")
