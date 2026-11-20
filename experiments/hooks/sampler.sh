#!/usr/bin/env bash
# usage: sampler.sh ROUNDS CALLS
cd /home/jcalabro/go/src/github.com/microscope-dev/microscope-hookexp
ms=$PWD/target/debug/microscope
burst=$PWD/build/test-programs/syscall-burst
out=/tmp/hookexp/sampler.txt
for round in $(seq 1 $1); do
  for ks in kernel user; do
    for mode in raw multi backstop; do
      flag=(); [[ $ks == kernel ]] && flag=(--kernel-stacks)
      f=/tmp/hookexp/s.$mode.$ks.$round.usprof
      ns=$(taskset -c 0,4 /tmp/hookexp/launcher-$mode.sh $ms record "${flag[@]}" --syscalls counts -o $f -- taskset -c 2 $burst $2 2>/tmp/hookexp/s.err | awk 'NF==2{print $2}' | tail -1)
      samples=$($ms report $f 2>&1 | rg -o "[0-9]+ samples, [0-9.]+ s" | head -1)
      echo "round=$round stacks=$ks mode=$mode ns=$ns $samples" >> $out
    done
  done
done
echo DONE >> $out
