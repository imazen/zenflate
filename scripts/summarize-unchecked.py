#!/usr/bin/env python3
"""Summarize the paired runs from compare-unchecked.sh; fail on missing cases."""
import csv
from pathlib import Path
import re
import statistics
import sys

root = Path(sys.argv[1])
enc = {}
dec = {}
for mode in ('safe', 'unchecked'):
    for run in (1, 2, 3):
        with (root / f'encode-{mode}-{run}.tsv').open() as source:
            rows = list(csv.DictReader(source, delimiter='\t'))
        assert rows, 'no encode cases'
        enc[mode, run] = {(r['image'], r['arm']): r for r in rows}
        assert len(enc[mode, run]) == len(rows), 'duplicate encode cases'
        decoded = {}
        for line in (root / f'decode-{mode}-{run}.txt').read_text().splitlines():
            if re.match(r'^\d{4}_', line):
                fields = line.split()
                decoded[fields[0]] = {'one-shot': float(fields[3]), 'streaming': float(fields[4])}
        dec[mode, run] = decoded

reference = enc['safe', 1]
images = {image for image, arm in reference}
expected_arms = set((root / 'arms.txt').read_text().strip().split(','))
assert {arm for image, arm in reference} == expected_arms, 'missing requested arms'
assert len(images) == 25, f'expected 25 images, got {len(images)}'
for key, rows in enc.items():
    assert rows.keys() == reference.keys(), f'encode cases differ: {key}'
    for case, row in rows.items():
        for field in ('bytes', 'filtered_bytes'):
            assert row[field] == reference[case][field], f'{field} differs: {key} {case}'
    assert dec[key].keys() == images, f'decode cases differ: {key}'

print('# Safe versus unchecked, ' + (root / 'date.txt').read_text().strip() + '\n')
print('Commit: `' + (root / 'commit.txt').read_text().strip() + '`.')
print('25 PNG variants from four source images; sizes 64, 256, 1024 and 2560,')
print('with gray8, RGB16 and RGBA8 variants at 1024. Three process runs per build;')
print('each encode case uses three timing rounds, each decode case nine.')
print('Safe/unchecked order alternates. Each table time is the median of the three')
print('per-run sums over images. U/S is their ratio; the range uses paired run ratios.')
print('Values below 1 mean unchecked took less time. Encoded sizes match in every case.\n')
print('| Operation | Safe ms | Unchecked ms | U/S | Paired range |')
print('|---|---:|---:|---:|---:|')

def report(label, safe, unchecked):
    s, u = statistics.median(safe), statistics.median(unchecked)
    ratios = [b / a for a, b in zip(safe, unchecked)]
    print(f'| {label} | {s / 1000:.3f} | {u / 1000:.3f} | {u / s:.4f} | {min(ratios):.4f}–{max(ratios):.4f} |')

for arm in sorted({arm for image, arm in reference}):
    sums = {mode: [sum(float(r['median_us']) for (image, a), r in enc[mode, run].items() if a == arm)
                   for run in (1, 2, 3)] for mode in ('safe', 'unchecked')}
    report(arm, sums['safe'], sums['unchecked'])
for arm in ('one-shot', 'streaming'):
    sums = {mode: [sum(r[arm] for r in dec[mode, run].values()) for run in (1, 2, 3)]
            for mode in ('safe', 'unchecked')}
    report(arm + ' decode', sums['safe'], sums['unchecked'])
