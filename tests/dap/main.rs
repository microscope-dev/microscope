//! Debug Adapter Protocol scenarios: the real `microscope dap` driven over
//! stdio the way editors drive it.

#[path = "../support/dap.rs"]
mod dap;
#[path = "../support/mod.rs"]
mod support;

mod breakpoints;
mod catchpoints;
mod chaos;
mod console;
mod differential;
mod execution;
mod faults;
#[cfg(debug_assertions)]
mod flight_recorder;
mod forks;
mod goroutines;
mod languages;
mod lifecycle;
mod memory;
mod node;
mod output;
mod panics;
mod python;
mod robustness;
mod server;
mod sources;
mod terminal;
mod tokio;
mod traffic;
mod variables;
mod watch;
mod writes;
