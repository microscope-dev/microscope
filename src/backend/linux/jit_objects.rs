use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nix::unistd::Pid;

use crate::debug_info::gdb_jit::{self, Registered};
use crate::{LoadBias, MemoryReadCompletion, VirtualAddress};

use super::Controller;
use super::memory::read_logical_block;
use super::native::LinuxTraceOps;

const MAX_TOTAL_BYTES: u64 = 64 << 20;

pub(super) type JitObjects = (Vec<(PathBuf, LoadBias)>, BTreeMap<PathBuf, Vec<u8>>);

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn observe_jit_objects(&self, pid: Pid) -> JitObjects {
        let mut observed = Vec::new();
        let mut objects = BTreeMap::new();
        let Some(inferior) = self.session.inferior() else {
            return (observed, objects);
        };

        let descriptors = self
            .loaded_images(inferior)
            .flat_map(|(module, image)| {
                image
                    .symbols_named(gdb_jit::DESCRIPTOR)
                    .filter_map(move |symbol| module.virtual_address(symbol.address()).ok())
                    .collect::<Vec<_>>()
            })
            .collect::<BTreeSet<_>>();
        let mut registered = Registered::default();
        let mut budget = MAX_TOTAL_BYTES;

        for descriptor in descriptors {
            let read = |address: u64, size: usize| {
                let read = read_logical_block(
                    &self.ptrace,
                    pid,
                    &inferior.breakpoints,
                    VirtualAddress::new(address),
                    size,
                )
                .ok()?;
                (read.completion == MemoryReadCompletion::Complete).then_some(read.bytes)
            };
            gdb_jit::registered(descriptor.get(), &mut budget, read, &mut registered);
        }

        #[cfg(debug_assertions)]
        for problem in &registered.problems {
            record!("{problem}");
        }

        for object in registered.objects {
            let name = object.name();
            observed.push((name.clone(), LoadBias::default()));
            objects.insert(name, object.bytes);
        }

        (observed, objects)
    }
}
