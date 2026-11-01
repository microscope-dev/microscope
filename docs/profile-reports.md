# Compare profile costs

Use a text report to compare two recordings of the same workload:

```sh
microscope report after.usprof --base before.usprof
microscope report after.usprof --base before.usprof --lines --limit 40
microscope report after.usprof --base before.usprof --tree --min-percent 0.5
```

The header compares total sampled cost, for example
`Total: 0.355 s → 0.118 s (-66.8%)`. The flat table shows each function's own
cost before and after, the difference (after minus before), and the percentage
change from before. It shows `new` for functions with no cost before and `gone`
for those with none after. Functions with zero own cost on both sides are
omitted. Rows sort by the size of the change, regardless of sign; positive
changes mean higher cost. `--lines` separates source lines; `--limit N` limits
the flat table.

CPU costs are in seconds. Other profiles keep their recorded unit, such as
executions or samples. Costs are absolute, not normalized for recording length;
percentage changes compare those costs. Compare equivalent work to measure a speedup.
Profiles with different units cannot be compared. `--waits` compares snapshot
wait time in seconds instead of CPU samples.

With `--tree`, the first four columns compare total cost through each path.
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

`--focus SYMBOL` matches an exact displayed name, as the browser does. For C++,
you can omit the parameter list:
`--focus content::RendererMain` finds
`content::RendererMain(content::MainFunctionParams)`. If several overloads
match, microscope lists them. If none match, it lists names containing the
search text, ignoring case.

The focus report shows the function's total and own cost, its immediate callers
with the cost that passed through them, and its immediate callees. This is
often called a butterfly view. `--limit N` limits each list.

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

To see where calls to a function come from, rather than what it calls, add
`--invert`:

```sh
microscope report reads.usprof --focus '[kernel: read]' --invert --tree
```

This prints callers as a tree, with each caller's callers nested beneath it up
to the outermost frame. The browser's "Inverted" checkbox shows the same view.
Use it to trace a system call's leaf frame past libc wrappers to the calling
code.

If several functions have the same displayed name, the command lists them
and refuses to choose. Copy a qualified name from that list, for example
`--focus 'leaf [spin.c:12]'`. A qualifier includes the module when known and
the full source path and declaration line when known. If those are also
identical, use `--lines` and an exact displayed line name, for example
`--lines --focus 'leaf spin.c:18'`.

When combined with `--base`, focus is resolved separately in each profile.
A function missing from the base contributes zero; an ambiguous name in
either profile is still an error. The butterfly lists and focused tree show
absolute before, after, and change columns, and the percentage change.

## Reshape the stacks

```sh
microscope report parse.usprof --tree --collapse-recursion
microscope report parse.usprof --tree --merge children --collapse self
```

These flags rewrite stacks before counting costs, in the order given. Each
`SYMBOL` resolves like `--focus`, including ambiguity errors. With `--base`,
names resolve in both profiles; a name missing from the base changes nothing
there.

- `--merge SYMBOL` removes the function from every stack. Its own cost goes
  to its caller, and its callees hang from that caller.
- `--drop SYMBOL` removes every sample whose stack holds the function.
- `--collapse SYMBOL` folds each run of the function calling itself directly
  into the run's first call.
- `--collapse-recursion` walks each stack from the outermost frame. When a
  function appears that is already on the stack, the stack is cut back to
  that earlier call, dropping the frames between. `self self self work`
  becomes `self work`, and `visit children visit children work` becomes
  `visit children work`.

Merge removes a function everywhere and moves its cost to callers. Collapse
removes repeated calls but keeps each function, moving own cost to its
outermost call so the tree has one level per function. Neither changes totals:
every sample still counts once. The browser's "Collapse recursion" button
matches `--collapse-recursion`; its per-function action matches `--collapse`.

See [profiling](profiling.md) for recording options and the browser workflow.
