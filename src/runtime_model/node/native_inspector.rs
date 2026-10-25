use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use super::RuntimeStop;
use crate::image::v8::NodeInspector;

pub(super) struct Server {
    pub agent: crate::VirtualAddress,
    pub address: SocketAddr,
    pub target: Arc<str>,
}

pub(super) fn target(
    layout: &NodeInspector,
    stop: &dyn RuntimeStop,
    caller: crate::VirtualAddress,
    delegate: crate::VirtualAddress,
) -> Result<Option<Arc<str>>, Arc<str>> {
    let caller = stop.load_bias().wrapping_unload(caller);
    if caller == layout.targets.upgrade {
        return Ok(None);
    }
    if !layout.targets.hidden.contains(&caller) {
        return Err("Node's inspector target caller is not a proven listing or upgrade".into());
    }
    let id = string(stop, checked(delegate.get(), layout.targets.source)?, 36)?;
    if id.len() != 36
        || !id.bytes().enumerate().all(|(at, byte)| {
            if matches!(at, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err("Node's inspector target is not a UUID".into());
    }
    Ok(Some(id.into()))
}

pub(super) fn startup_pending(
    layout: &NodeInspector,
    stop: &dyn RuntimeStop,
    agent: crate::VirtualAddress,
) -> Result<bool, Arc<str>> {
    let startup = layout
        .startup
        .ok_or("Node's startup async layout is unavailable")?;
    let handle = if layout.vector {
        word(stop, checked(agent.get(), startup.async_offset)?)?
    } else {
        stop.load_bias().wrapping_load(layout.registry).get()
    };
    if handle == 0 || word(stop, handle)? != agent.get() {
        return Err("Node's startup async handle no longer belongs to the created agent".into());
    }
    // Linux x86-64 UV_HANDLE_FIELDS + UV_ASYNC_PRIVATE_FIELDS (uv/unix.h).
    // The fixture checks data=0, pending=120 and sizeof(uv_async_t)=128.
    let mut bytes = [0; 4];
    if !stop.read(
        crate::VirtualAddress::new(checked(handle, 120)?),
        &mut bytes,
    ) {
        return Err("Node's startup async pending flag is unreadable".into());
    }
    match u32::from_le_bytes(bytes) {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err("Node's startup async pending flag is invalid".into()),
    }
}

pub(super) fn endpoint(
    layout: &NodeInspector,
    stop: &dyn RuntimeStop,
) -> Result<Option<(SocketAddr, Arc<str>)>, Arc<str>> {
    server(layout, stop).map(|server| server.map(|server| (server.address, server.target)))
}

pub(super) fn server(
    layout: &NodeInspector,
    stop: &dyn RuntimeStop,
) -> Result<Option<Server>, Arc<str>> {
    let registry = stop.load_bias().wrapping_load(layout.registry).get();
    let agents = if layout.vector {
        let begin = word(stop, registry)?;
        let end = word(stop, checked(registry, 8)?)?;
        let length = end
            .checked_sub(begin)
            .ok_or("Node's agent registry is reversed")?;
        if length % 8 != 0 || length > 64 * 8 {
            return Err("Node's agent registry is not a bounded vector of pointers".into());
        }
        (0..length / 8)
            .map(|index| word(stop, checked(begin, index * 8)?))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        // uv_handle_t's public UV_HANDLE_FIELDS starts with void* data.
        vec![word(stop, registry)?]
    };
    let mut found = None;
    for agent in agents.into_iter().filter(|agent| *agent != 0) {
        let io = word(stop, checked(agent, layout.io)?)?;
        if io == 0 {
            continue;
        }
        let host_port = word(stop, checked(io, layout.host_port)?)?;
        let host = string(stop, checked(host_port, layout.host)?, 64)?;
        let ip: IpAddr = host
            .parse()
            .map_err(|_| "Node's inspector host is not a numeric IP address")?;
        if !ip.is_loopback() {
            return Err("Node's inspector does not listen on a loopback address".into());
        }
        let mut port = [0; 2];
        if !stop.read(
            crate::VirtualAddress::new(checked(host_port, layout.port)?),
            &mut port,
        ) {
            return Err("Node's inspector port is unreadable".into());
        }
        let port = u16::from_le_bytes(port);
        if port == 0 {
            return Err("Node's inspector has not published its listening port".into());
        }
        let id = string(stop, checked(io, layout.id)?, 36)?;
        if id.len() != 36
            || !id.bytes().enumerate().all(|(at, byte)| {
                if matches!(at, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
        {
            return Err("Node's inspector target is not a UUID".into());
        }
        if found
            .replace(Server {
                agent: crate::VirtualAddress::new(agent),
                address: SocketAddr::new(ip, port),
                target: id.into(),
            })
            .is_some()
        {
            return Err("more than one native inspector server belongs to this Node image".into());
        }
    }
    Ok(found)
}

pub(super) fn message_matches(
    stop: &dyn RuntimeStop,
    view: crate::VirtualAddress,
    expected: &[u8],
) -> Result<bool, Arc<str>> {
    // v8-inspector.h's StringView is bool, size_t, then a pointer union.
    // The x86-64 ABI layout is checked against each pinned V8 header.
    let mut is8 = [0];
    if !stop.read(view, &mut is8) {
        return Err("the inspector dispatch's StringView is unreadable".into());
    }
    let length = word(stop, checked(view.get(), 8)?)?;
    if length != u64::try_from(expected.len()).map_err(|_| "cleanup message is too large")? {
        return Ok(false);
    }
    let data = word(stop, checked(view.get(), 16)?)?;
    let width = match is8[0] {
        0 => 2,
        1 => 1,
        _ => return Err("the inspector dispatch's StringView has an invalid encoding".into()),
    };
    let mut bytes = vec![0; expected.len() * width];
    if !stop.read(crate::VirtualAddress::new(data), &mut bytes) {
        return Err("the inspector dispatch's message is unreadable".into());
    }
    Ok(if width == 1 {
        bytes == expected
    } else {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .zip(expected)
            .all(|(unit, byte)| *unit == [*byte, 0])
    })
}

fn checked(base: u64, offset: u64) -> Result<u64, Arc<str>> {
    base.checked_add(offset)
        .ok_or_else(|| "Node's inspector address overflows".into())
}

fn word(stop: &dyn RuntimeStop, address: u64) -> Result<u64, Arc<str>> {
    let mut bytes = [0; 8];
    if !stop.read(crate::VirtualAddress::new(address), &mut bytes) {
        return Err("Node's native inspector state is unreadable".into());
    }
    Ok(u64::from_le_bytes(bytes))
}

fn string(stop: &dyn RuntimeStop, address: u64, maximum: usize) -> Result<String, Arc<str>> {
    let data = word(stop, address)?;
    let length = usize::try_from(word(stop, checked(address, 8)?)?)
        .map_err(|_| "Node's inspector string length overflows")?;
    if length == 0 || length > maximum {
        return Err("Node's inspector string has an unsupported length".into());
    }
    let mut bytes = vec![0; length];
    if !stop.read(crate::VirtualAddress::new(data), &mut bytes) {
        return Err("Node's inspector string is unreadable".into());
    }
    String::from_utf8(bytes).map_err(|_| "Node's inspector string is not UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LoadBias, ThreadId, VirtualAddress};

    struct Heap(Vec<u8>);

    impl RuntimeStop for Heap {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            let Ok(at) = usize::try_from(address.get()) else {
                return false;
            };
            let Some(data) = self.0.get(at..at.saturating_add(bytes.len())) else {
                return false;
            };
            bytes.copy_from_slice(data);
            true
        }
        fn thread_pointer(&self, _: ThreadId) -> Option<u64> {
            None
        }
        fn instruction(&self, _: ThreadId) -> Option<VirtualAddress> {
            None
        }
        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }
        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    fn layout() -> NodeInspector {
        NodeInspector {
            registry: crate::ImageAddress::new(128),
            signal_handler: crate::ImageAddress::new(1),
            vector: false,
            io: 0,
            host_port: 0,
            host: 0,
            port: 0,
            id: 0,
            close: None,
            startup: Some(crate::image::v8::InspectorStartup {
                before_wait: crate::ImageAddress::new(1),
                after_wait: crate::ImageAddress::new(2),
                check: crate::ImageAddress::new(3),
                async_offset: 8,
            }),
            targets: crate::image::v8::InspectorTargets {
                entry: crate::ImageAddress::new(4),
                source: 8,
                hidden: [5, 6, 7].map(crate::ImageAddress::new),
                upgrade: crate::ImageAddress::new(8),
            },
        }
    }

    #[test]
    fn target_privacy_requires_a_known_caller_and_reads_the_unpublished_delegates_uuid() {
        let mut layout = layout();
        let mut heap = Heap(vec![0; 128]);
        let delegate = VirtualAddress::new(16);
        let id = b"01234567-89ab-4def-8123-0123456789ab";
        heap.0[24..32].copy_from_slice(&64u64.to_le_bytes());
        heap.0[32..40].copy_from_slice(&36u64.to_le_bytes());
        heap.0[64..100].copy_from_slice(id);
        for caller in layout.targets.hidden {
            let target =
                target(&layout, &heap, VirtualAddress::new(caller.get()), delegate).unwrap();
            assert_eq!(target.unwrap().as_bytes(), id);
        }
        assert!(
            target(&layout, &Heap(Vec::new()), VirtualAddress::new(8), delegate)
                .unwrap()
                .is_none()
        );
        assert!(target(&layout, &heap, VirtualAddress::new(9), delegate).is_err());
        layout.targets.source = 16;
        assert!(target(&layout, &heap, VirtualAddress::new(5), delegate).is_err());
        layout.targets.source = 8;
        heap.0[64] = b'!';
        assert!(target(&layout, &heap, VirtualAddress::new(5), delegate).is_err());
    }

    #[test]
    fn startup_pending_checks_the_handle_owner_encoding_and_bounds() {
        for vector in [false, true] {
            let mut heap = Heap(vec![0; 512]);
            let mut layout = layout();
            layout.vector = vector;
            let agent = VirtualAddress::new(16);
            heap.0[24..32].copy_from_slice(&128u64.to_le_bytes());
            heap.0[128..136].copy_from_slice(&agent.get().to_le_bytes());
            assert!(!startup_pending(&layout, &heap, agent).unwrap());
            heap.0[248] = 1;
            assert!(startup_pending(&layout, &heap, agent).unwrap());
            heap.0[248] = 2;
            assert!(startup_pending(&layout, &heap, agent).is_err());
            heap.0[248] = 0;
            heap.0[128] ^= 1;
            assert!(startup_pending(&layout, &heap, agent).is_err());
            heap.0[128] ^= 1;
            if vector {
                layout.startup.as_mut().unwrap().async_offset = 16;
                assert!(startup_pending(&layout, &heap, agent).is_err());
            }
            heap.0.truncate(250);
            assert!(startup_pending(&layout, &heap, agent).is_err());
            layout.startup = None;
            assert!(startup_pending(&layout, &heap, agent).is_err());
        }
    }

    #[test]
    fn dispatch_string_view_matches_only_the_exact_narrow_or_wide_nonce() {
        let expected = br#"{"id":42,"method":"Runtime.getIsolateId"}"#;
        for wide in [false, true] {
            let mut heap = Heap(vec![0; 512]);
            heap.0[0] = u8::from(!wide);
            heap.0[8..16].copy_from_slice(&(expected.len() as u64).to_le_bytes());
            heap.0[16..24].copy_from_slice(&32u64.to_le_bytes());
            for (index, byte) in expected.iter().enumerate() {
                heap.0[32 + index * if wide { 2 } else { 1 }] = *byte;
            }
            assert!(message_matches(&heap, VirtualAddress::new(0), expected).unwrap());
            heap.0[32] ^= 1;
            assert!(!message_matches(&heap, VirtualAddress::new(0), expected).unwrap());
            heap.0[0] = 2;
            assert!(message_matches(&heap, VirtualAddress::new(0), expected).is_err());
            heap.0[0] = u8::from(!wide);
            heap.0[16..24].copy_from_slice(&500u64.to_le_bytes());
            assert!(message_matches(&heap, VirtualAddress::new(0), expected).is_err());
        }
    }

    #[test]
    fn string_view_layout_matches_every_pinned_header_with_both_compilers() {
        for version in ["12.4", "13.6", "14.6"] {
            for compiler in ["g++", "clang++"] {
                let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("build/test-programs")
                    .join(format!("inspector-string-view-{version}-{compiler}"));
                assert!(
                    std::process::Command::new(fixture)
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
    }
}
