# Compare profile costs

Use a text report to compare two recordings of the same workload:

```sh
microscope report after.usprof --base before.usprof
microscope report after.usprof --base before.usprof --lines --limit 40
microscope report after.usprof --base before.usprof --tree --min-percent 0.5
```

The header shows each profile's total sampled cost. The flat table shows each
function's own cost before and after, followed by after minus before. Rows are
sorted by the size of that change, regardless of its sign. Positive changes
mean more cost. `--lines` separates source lines and `--limit N` limits the
flat table.

CPU costs are seconds. Other profiles keep their recorded unit, such as
executions or samples. Costs are not percentages or normalized to equal run
lengths. Compare equivalent work if you want to measure a speedup. Profiles
with different units cannot be compared. `--waits` compares snapshot wait
time, in seconds, instead of CPU samples.

With `--tree`, the first three columns compare total cost through each path.
The next three compare its own cost. Functions called from several places
still appear under each caller. Children are sorted by the size of their
total-cost change. `--min-percent` keeps a node if its total reaches the
threshold in either recording, so a removed branch can still appear. Trees
are not limited by `--limit`.

Names and paths are matched the same way as in the browser. A function absent
from one recording has zero cost on that side. Matching by name cannot prove
that two unrelated functions with the same name are the same code.

See [profiling](profiling.md) for recording options and the browser workflow.
