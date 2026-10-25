# microscope for Node programs

The `microscope` package marks a Node program's work for microscope's
recorder: zones, labels, plots, messages, frames, and triggers. See
[Instrument a Node program](../../docs/profiling.md#instrument-a-node-program).

`index.js` is the package. Its addon, `microscope.node`, is a Node-API
module, so one build serves every Node release, and it loads the probes
SDK built as a shared library. Build both next to `index.js`:

```sh
gcc -std=c17 -O2 -shared -fPIC -I"$(dirname "$(command -v node)")/../include/node" \
    microscope.c -o microscope.node -ldl -pthread
(cd ../rust-probes && cargo rustc --release --features c --crate-type cdylib)
cp ../rust-probes/target/release/libmicroscope_probes.so .
```

`MICROSCOPE_NODE_ADDON` names the addon and `MICROSCOPE_PROBES_LIBRARY` the
library when they are elsewhere. Without either the package still loads,
every mark does nothing, and `microscope.problem` says why.
