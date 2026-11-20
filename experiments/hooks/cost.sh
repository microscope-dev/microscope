#!/usr/bin/env bash
# usage: cost.sh ROUNDS CALLS
cd /home/jcalabro/go/src/github.com/microscope-dev/microscope-hookexp
ms=$PWD/target/debug/microscope
burst=$PWD/build/test-programs/syscall-burst
out=/tmp/hookexp/cost.txt
loop() { taskset -c 2 "$burst" "$2" | awk '{print $2}'; }
for round in $(seq 1 $1); do
  echo "round=$round setup=bare ns=$(taskset -c 2 $burst $2 | awk '{print $2}')" >> $out
  for mode in raw multi backstop; do
    l=/tmp/hookexp/launcher-$mode.sh
    # untargeted: another program is recorded; the loop is not
    fifo=/tmp/hookexp/fifo.$$; rm -f $fifo; mkfifo $fifo
    taskset -c 0,4 $l $ms record --syscalls calls -o /dev/null -- sh -c 'echo ready; read line' < $fifo > /tmp/hookexp/untargeted.out 2>&1 &
    rec=$!
    exec 9> $fifo
    for i in $(seq 1 100); do rg -q ready /tmp/hookexp/untargeted.out 2>/dev/null && break; sleep 0.1; done
    sleep 0.5
    echo "round=$round setup=untargeted mode=$mode ns=$(taskset -c 2 $burst $2 | awk '{print $2}')" >> $out
    echo >&9; exec 9>&-; wait $rec; rm -f $fifo
    # counts and calls: the loop itself is recorded
    v=$(taskset -c 0,4 $l $ms record --preset prod --syscalls counts -o /dev/null -- taskset -c 2 $burst $2 2>/dev/null | awk 'NF==2{print $2}' | tail -1)
    echo "round=$round setup=counts mode=$mode ns=$v" >> $out
    v=$(taskset -c 0,4 $l $ms record --preset dev --syscalls calls -o /dev/null -- taskset -c 2 $burst $2 2>/dev/null | awk 'NF==2{print $2}' | tail -1)
    echo "round=$round setup=calls mode=$mode ns=$v" >> $out
  done
done
echo DONE >> $out
