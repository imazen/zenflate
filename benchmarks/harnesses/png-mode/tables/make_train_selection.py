"""Training selection for the ultra-table experiments: the imazen-26 K500
cluster representatives whose source image is in the train split.

usage: python3 -I make_train_selection.py <imazen-26 checkout> > train_selection.tsv

Disjoint from the held-out ladder set (selection.tsv: K300 validate + test).
Columns match selection.tsv.
"""
import csv, os, sys

root = sys.argv[1]
man = os.path.join(root, 'manifests')
split = {r['id']: r['split'] for r in csv.DictReader(open(os.path.join(man, 'split_map.tsv')), delimiter='\t')}
prefix = 'https://codec-corpus.r2.imazen.org/imazen-26-png-v3/'
w = csv.writer(sys.stdout, delimiter='\t', lineterminator='\n')
w.writerow(['id', 'split', 'class', 'crop', 'cluster_id', 'cluster_size', 'path'])
for r in csv.DictReader(open(os.path.join(man, 'imazen26_representatives_K500_2026-06-14.tsv')), delimiter='\t'):
    path = r['url'][len(prefix):]
    id_ = os.path.basename(path).split('_')[0]
    if split.get(id_) == 'train':
        w.writerow([id_, 'train', r['content_class'], r['crop_label'], r['cluster_id'], r['cluster_size'], path])
