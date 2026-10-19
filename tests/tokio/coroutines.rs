//! microscope's reading of each coroutine's debug information, compared with
//! readelf's: the build reduces readelf's dump of each fixture to one line
//! per coroutine state, beside the fixture as `<fixture>.coroutines`.

use std::collections::BTreeSet;

use microscope::{CoroutineStateKind, ModuleImage, RecordMemberLayout, TypeNode};

use crate::support::Scenario;

/// One state as readelf's record writes it: the coroutine's path and name,
/// where its state number is, the state's number and record's name, its
/// line, and the type of the future it awaits.
type StateLine = (String, u64, u64, String, u64, String);

fn readelf_record(fixture: &str) -> BTreeSet<StateLine> {
    let path = Scenario::fixture(fixture).with_extension("coroutines");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

    text.lines()
        .map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();

            let [name, offset, state, record, declared, awaited] = fields[..] else {
                panic!("{}: malformed line {line:?}", path.display());
            };

            let number = |field: &str| {
                field
                    .parse()
                    .unwrap_or_else(|_| panic!("{}: {field:?} in {line:?}", path.display()))
            };

            (
                name.to_owned(),
                number(offset),
                number(state),
                record.to_owned(),
                number(declared),
                awaited.to_owned(),
            )
        })
        .collect()
}

/// The same lines from microscope's normalization of every coroutine type.
fn normalized(image: &ModuleImage) -> BTreeSet<StateLine> {
    let mut lines = BTreeSet::new();
    for node in image.types() {
        let TypeNode::Resolved(info) = node else {
            continue;
        };

        let Some(coroutine) = image.coroutine(info.reference.id) else {
            continue;
        };

        let identity = info.identity.as_ref().expect("a coroutine has a path");
        let mut name = identity.path.join("::");
        if !name.is_empty() {
            name.push_str("::");
        }

        name.push_str(&info.name);

        let coroutine = coroutine.unwrap_or_else(|reason| panic!("{name}: {reason}"));
        for state in coroutine.states.iter() {
            let record = match state.kind {
                CoroutineStateKind::Unresumed => "Unresumed".to_owned(),
                CoroutineStateKind::Returned => "Returned".to_owned(),
                CoroutineStateKind::Panicked => "Panicked".to_owned(),
                CoroutineStateKind::Suspended { index } => format!("Suspend{index}"),
            };

            let awaited = state
                .saved
                .iter()
                .find(|member| member.name.as_deref() == Some("__awaitee"))
                .and_then(|member| image.type_info(member.type_ref))
                .map_or_else(|| "-".to_owned(), |awaited| awaited.name.to_string());

            lines.insert((
                name.clone(),
                coroutine.state.offset,
                state.value,
                record,
                state
                    .location
                    .as_ref()
                    .map_or(0, |location| location.line.get()),
                awaited,
            ));
        }

        // Every state's members are laid out within the coroutine.
        for member in coroutine.states.iter().flat_map(|state| state.saved.iter()) {
            assert!(
                matches!(member.layout, RecordMemberLayout::ByteOffset(at)
                    if info.byte_size.is_some_and(|size| at < size)),
                "{name}: {member:?}"
            );
        }
    }

    lines
}

#[tokio::test]
async fn coroutines_read_as_readelf_reads_them() {
    for fixture in ["tokio-std-async-o0", "tokio-std-async-o3"] {
        let scenario = Scenario::launch(fixture);
        let expected = readelf_record(fixture);
        assert!(
            !expected.is_empty(),
            "{fixture}: readelf found no coroutine"
        );

        let actual = normalized(scenario.handle().module_image());
        let missing = expected.difference(&actual).collect::<Vec<_>>();
        let extra = actual.difference(&expected).collect::<Vec<_>>();

        assert!(
            missing.is_empty() && extra.is_empty(),
            "{fixture}: readelf only: {missing:#?}\nmicroscope only: {extra:#?}"
        );

        scenario.shutdown().await;
    }
}
