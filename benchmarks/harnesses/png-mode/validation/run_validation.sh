#!/bin/bash
# Held-out validation of png(1..17): run from benchmarks/harnesses/png-mode after
#   python3 -I validation/make_variants.py   (fetches nothing; expects the selection's
#   source PNGs under $LADDER_ROOT/img/<class>/, see the script)
set -u
ROOT=${LADDER_ROOT:-$HOME/tmp/ladder-corpus}
B=./target/release/png-mode-bench
export IMG_PER_CLASS=1000 CROP_PX=100000000 PER_IMAGE=1 TRANSFORMS=png_adaptive,png_none
export PNG_EFFORTS=1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17
export ZF_EFFORTS=1,5,9,10,12
export CODEC_FILTER=ref-store,png,zf,fd-ultrafast,fd-L1,fd-L2,fd-L3
for bucket in 64 256 native; do
  case $bucket in 64) reps=21;; 256) reps=11;; native) reps=5;; esac
  IMG_CORPUS_DIR=$ROOT/sizes/$bucket REPS=$reps nice -n 19 "$B" > v3-$bucket.csv 2> v3-$bucket.log
done
python3 -I validation/ladder_table.py v3 png-e01,png-e02,png-e03,png-e04,png-e05,png-e06,png-e07,png-e08,png-e09,png-e10,png-e11,png-e12,png-e13,png-e14,png-e15
