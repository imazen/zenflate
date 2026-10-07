"""DEFLATE cost model for the ultra-table experiments (bits, not bytes).

Header bits follow zenflate's write_dynamic_header_body (libdeflate's RLE of
the code lengths, precode limited to 7 bits) for a 286-symbol literal/length
code plus the ultra encoder's distance code (two 1-bit codes). Tables come
from package-merge, which is optimal under the length limit; zenflate's
encoder builds its tables with libdeflate's heuristic, so modeled sizes are
estimates, not encoder output.
"""
import numpy as np

NSYM = 286
PERM = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15]
EXTRA_PRECODE = [0] * 16 + [2, 3, 7]
STATIC = np.array([8] * 144 + [9] * 112 + [7] * 24 + [8] * 6, dtype=np.float64)  # BTYPE=01 litlen
BLOCK = 256 * 1024  # ultra block length


def package_merge(freqs, maxlen):
    """Optimal length-limited code lengths for symbols with freq > 0 (others get 0)."""
    n = len(freqs)
    used = sorted((f, i) for i, f in enumerate(freqs) if f > 0)
    lens = [0] * n
    if not used:
        return lens
    if len(used) == 1:
        lens[used[0][1]] = 1
        return lens
    leaves = [(f, [i]) for f, i in used]
    current = list(leaves)
    for _ in range(maxlen - 1):
        packages = [(current[j][0] + current[j + 1][0], current[j][1] + current[j + 1][1])
                    for j in range(0, len(current) - 1, 2)]
        current = sorted(leaves + packages, key=lambda t: t[0])
    for _, syms in current[:2 * len(used) - 2]:
        for s in syms:
            lens[s] += 1
    return lens


def precode_items(lens):
    """libdeflate's run-length coding of a code-length sequence: precode symbols."""
    items = []
    i, n = 0, len(lens)
    while i < n:
        l = lens[i]
        j = i + 1
        while j < n and lens[j] == l:
            j += 1
        if l == 0:
            while j - i >= 11:
                items.append(18); i += 11 + min(j - i - 11, 0x7F)
            while j - i >= 3:
                items.append(17); i += 3 + min(j - i - 3, 7)
        elif j - i >= 4:
            items.append(l); i += 1
            while j - i >= 3:
                items.append(16); i += 3 + min(j - i - 3, 3)
        while i < j:
            items.append(l); i += 1
    return items


_hdr_cache = {}


def header_bits(litlen_lens):
    """Dynamic header bits (after BFINAL/BTYPE) for these 286 literal/length lengths."""
    key = tuple(int(x) for x in litlen_lens)
    if key not in _hdr_cache:
        nl = 286
        while nl > 257 and key[nl - 1] == 0:
            nl -= 1
        items = precode_items(list(key[:nl]) + [1, 1])
        freqs = [0] * 19
        for s in items:
            freqs[s] += 1
        plens = package_merge(freqs, 7)
        nexp = 19
        while nexp > 4 and plens[PERM[nexp - 1]] == 0:
            nexp -= 1
        _hdr_cache[key] = 5 + 5 + 4 + 3 * nexp + sum(freqs[s] * (plens[s] + EXTRA_PRECODE[s]) for s in range(19))
    return _hdr_cache[key]


def build(counts, maxlen=12, prior=1e-9):
    """Length-limited code from (normalized or raw) counts; every symbol gets a code."""
    f = np.asarray(counts, dtype=np.float64)
    f = f / max(f.sum(), 1e-300) + prior
    w = np.maximum(1, np.round(f / f.min())).astype(np.int64)
    return np.array(package_merge([int(x) for x in w], maxlen), dtype=np.float64)


def nblocks(nbytes):
    return np.maximum(1, -(-np.asarray(nbytes) // BLOCK))
