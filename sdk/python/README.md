# microscope for Python programs

The `microscope` package marks a Python program's work for microscope's
recorder: zones, labels, plots, messages, frames, and triggers, and
`trace_calls`, which times the calls of chosen modules. See
[Instrument a Python program](../../docs/profiling.md#instrument-a-python-program).

`microscope/__init__.py` is the whole package. It loads the probes SDK built
as a shared library:

```sh
cd sdk/rust-probes
cargo rustc --release --features c --crate-type cdylib
cp target/release/libmicroscope_probes.so ../python/microscope/
```
