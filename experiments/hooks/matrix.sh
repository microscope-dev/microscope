#!/usr/bin/env bash
# usage: matrix.sh ROUNDS
cd /home/jcalabro/go/src/github.com/microscope-dev/microscope-hookexp
out=/tmp/hookexp/matrix.txt
for round in $(seq 1 $1); do
  for load in 0 16; do
    pids=()
    for i in $(seq 1 $load); do dd if=/dev/zero of=/dev/null bs=1 2>/dev/null & pids+=($!); done
    for mode in raw multi backstop; do
      rm -f /tmp/hookexp/misses.*
      log=/tmp/hookexp/b1.$mode.$load.$round.log
      just test-bpf /tmp/hookexp/launcher-$mode.sh over_budget_a_recording_sheds_calls_before_samples --no-capture > $log 2>&1
      code=$?
      res=$(rg -o "EXPERIMENT .*" $log | head -1)
      m=$(cat /tmp/hookexp/misses.* 2>/dev/null | rg -v " 0$" | tr '\n' ' ')
      echo "round=$round load=$load mode=$mode exit=$code | $m| $res" >> $out
    done
    for p in "${pids[@]}"; do kill $p; done
    wait 2>/dev/null
  done
done
echo DONE >> $out
