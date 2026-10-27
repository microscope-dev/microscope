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

## Ask about one function

```sh
microscope report spin.usprof --focus leaf
microscope report spin.usprof --focus leaf --tree
microscope report after.usprof --base before.usprof --focus leaf --tree
```

`--focus SYMBOL` matches an exact displayed name, as the browser does. The
ordinary focus report shows the function's total and own cost, its immediate
callers with the cost that passed through them, and its immediate callees.
This is often called a butterfly view. `--limit N` limits each list.

Each sample counts once in the function's total, even with recursion. Its own
cost includes samples that end in any occurrence of the function. Caller and
callee lists describe the outermost occurrence on each stack, the same one
the browser's focus selects. A recursive call can therefore appear as a
callee, but its cost is not added to the function's total a second time.
Costs in these lists are not call counts. A focused root has no caller;
a leaf has no callees.

With `--tree`, all paths from the selected function are merged into one
subtree. Recursive calls remain in that subtree. Other callers' paths are
not printed, and `--min-percent` is relative to the focused subtree.

If several functions have the same displayed name, the command lists them
and refuses to choose. Copy a qualified name from that list, for example
`--focus 'leaf [spin.c:12]'`. A qualifier includes the module when known and
the full source path and declaration line when known. If those are also
identical, use `--lines` and an exact displayed line name, for example
`--lines --focus 'leaf spin.c:18'`.

When combined with `--base`, focus is resolved separately in each profile.
A function missing from the base contributes zero; an ambiguous name in
either profile is still an error. The butterfly lists and focused tree show
absolute before, after, and change columns.

See [profiling](profiling.md) for recording options and the browser workflow.
