use std::collections::BTreeMap;

use anyhow::{Result, anyhow};

use crate::backend::bpf::sampler;
use crate::backend::syscall_engines::bpf;
use crate::syscalls::filter::Filter;

pub fn measure(filter: &str) -> Result<BTreeMap<String, u64>> {
    let calls = Filter::parse(&[filter.to_owned()], &|_| None)
        .map_err(|error| anyhow!(error.0))?
        .calls;
    let mut verified: BTreeMap<String, u64> =
        bpf::verified_instructions(calls)?.into_iter().collect();
    verified.insert(
        "python_sample".to_owned(),
        sampler::verified_instructions(false)?,
    );
    verified.insert(
        "python_sample with kernel samples".to_owned(),
        sampler::verified_instructions(true)?,
    );
    Ok(verified)
}
