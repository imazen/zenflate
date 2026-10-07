"""Score candidate ladders: aggregate ratio/speed + per-image monotonicity inversions."""
import csv, collections, sys
sizes_log, agg_csv = sys.argv[1], sys.argv[2]
d = collections.defaultdict(dict)
for row in csv.reader(open(sizes_log)):
    if row and row[0] == "SIZE":
        _, t, img, c, n = row
        d[(t, img)][c] = int(n)
agg = {}
for r in csv.DictReader(open(agg_csv)):
    if r["class"] == "*ALL":
        agg[(r["transform"], r["codec"])] = (float(r["ratio"]), float(r["mib_s"]))
def score(chain):
    print(" -> ".join(chain))
    for t in ("png_adaptive", "png_none"):
        imgs = [k for k in d if k[0] == t]
        line = []
        for a, b in zip(chain, chain[1:]):
            worse = [(d[k][b] - d[k][a]) / d[k][a] * 100 for k in imgs if d[k][b] > d[k][a]]
            line.append(f"{b}:{len(worse)}/{max(worse) if worse else 0:.2f}%")
        print(f"  {t:13s} inversions " + "  ".join(line))
        print(f"  {t:13s} " + "  ".join(f"{c}={agg[(t,c)][0]:.4f}@{agg[(t,c)][1]:.0f}" for c in chain))
for spec in sys.argv[3:]:
    score(spec.split(","))
