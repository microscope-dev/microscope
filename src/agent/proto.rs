//! The agent protocol's messages, generated from
//! `proto/microscope/agent/v1/agent.proto` by the build script.

#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    reason = "prost generates this code"
)]

include!(concat!(env!("OUT_DIR"), "/microscope.agent.v1.rs"));
