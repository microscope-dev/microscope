use std::collections::BTreeMap;

use super::*;

use super::test_image as image;

/// A process's memory: each region by where it starts.
#[derive(Default)]
struct Memory(BTreeMap<u64, Vec<u8>>);

impl Memory {
    fn put(&mut self, address: u64, bytes: Vec<u8>) {
        self.0.insert(address, bytes);
    }

    fn descriptor(&mut self, address: u64, version: u32, first: u64) {
        let mut bytes = version.to_le_bytes().to_vec();
        bytes.extend([0; 12]);
        bytes.extend(first.to_le_bytes());
        self.put(address, bytes);
    }

    fn entry(&mut self, address: u64, next: u64, object: u64, size: usize) {
        let bytes = [next, 0, object, size as u64]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        self.put(address, bytes);
    }

    fn read(&self, address: u64, size: usize) -> Option<Vec<u8>> {
        let (start, bytes) = self.0.range(..=address).next_back()?;
        let at = usize::try_from(address - start).ok()?;
        bytes.get(at..at + size).map(<[u8]>::to_vec)
    }

    fn registered(&self, descriptor: u64, mut budget: u64) -> Registered {
        let mut read = Registered::default();
        registered(
            descriptor,
            &mut budget,
            |address, size| self.read(address, size),
            &mut read,
        );
        read
    }
}

/// Each object a sound list holds is read, in its order, named for where
/// it lies and what it holds, with its code where its sections say. An
/// object that cannot be read is left out and said; a list that cannot be
/// followed on is read as far as it goes, and said.
#[test]
fn registered_objects_are_read_as_far_as_the_list_is_sound() {
    let (first, second) = (image(0x7000_1000, 64), image(0x7000_2000, 32));
    let mut memory = Memory::default();
    memory.descriptor(0x1000, 1, 0x2000);
    memory.entry(0x2000, 0x2100, 0x3000, first.len());
    memory.entry(0x2100, 0, 0x4000, second.len());
    memory.put(0x3000, first.clone());
    memory.put(0x4000, second.clone());

    let read = memory.registered(0x1000, u64::MAX);
    assert_eq!(read.problems, Vec::<String>::new());
    let addresses: Vec<u64> = read.objects.iter().map(|object| object.address).collect();
    assert_eq!(addresses, [0x3000, 0x4000]);
    assert_eq!(read.objects[0].code(), Some(0x7000_1000..0x7000_1040));
    assert_eq!(read.objects[1].code(), Some(0x7000_2000..0x7000_2020));
    assert_ne!(read.objects[0].name(), read.objects[1].name());

    // The same place with other contents is another object.
    let other = JitObject {
        address: 0x3000,
        bytes: image(0x7000_1000, 48),
    };
    assert_ne!(other.name(), read.objects[0].name());

    // The second object no longer fits what is left to read.
    let budget = first.len() as u64 + 1;
    let read = memory.registered(0x1000, budget);
    assert_eq!(read.objects.len(), 1);
    assert!(
        read.problems[0].contains("left to read"),
        "{:?}",
        read.problems
    );

    // A list that comes back on itself.
    memory.entry(0x2100, 0x2000, 0x4000, second.len());
    let read = memory.registered(0x1000, u64::MAX);
    assert_eq!(read.objects.len(), 2);
    assert!(read.problems[0].contains("loops"), "{:?}", read.problems);

    // An object that is no image, and an entry that cannot be read.
    memory.entry(0x2000, 0x2100, 0x3000, first.len());
    memory.entry(0x2100, 0x2200, 0x4000, second.len());
    memory.put(0x4000, vec![0; second.len()]);
    let read = memory.registered(0x1000, u64::MAX);
    assert_eq!(read.objects.len(), 1);
    assert_eq!(read.problems.len(), 2, "{:?}", read.problems);
    assert!(read.problems[0].contains("no ELF image"));
    assert!(read.problems[1].contains("entry at 0x2200 is unreadable"));

    // An object of no size, or of more than is ever read.
    memory.entry(0x2000, 0x2100, 0x3000, 0);
    memory.entry(
        0x2100,
        0,
        0x4000,
        usize::try_from(MAX_OBJECT_BYTES + 1).expect("a size"),
    );
    let read = memory.registered(0x1000, u64::MAX);
    assert!(read.objects.is_empty());
    assert_eq!(read.problems.len(), 2, "{:?}", read.problems);
}

/// A descriptor of another version, or unreadable, gives nothing and says
/// why; one a compiler never used gives nothing and has nothing to say.
#[test]
fn only_a_descriptor_of_the_known_version_is_read() {
    let mut memory = Memory::default();
    memory.descriptor(0x1000, 2, 0x2000);
    let read = memory.registered(0x1000, u64::MAX);
    assert!(read.objects.is_empty());
    assert!(
        read.problems[0].contains("version Some(2)"),
        "{:?}",
        read.problems
    );

    memory.descriptor(0x1000, 0, 0);
    assert_eq!(memory.registered(0x1000, u64::MAX), Registered::default());

    memory.descriptor(0x1000, 0, 0x2000);
    assert_eq!(memory.registered(0x1000, u64::MAX).problems.len(), 1);

    let read = memory.registered(0x9000, u64::MAX);
    assert!(
        read.problems[0].contains("unreadable"),
        "{:?}",
        read.problems
    );
}
