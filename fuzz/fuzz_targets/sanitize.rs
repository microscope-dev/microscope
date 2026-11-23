#![no_main]

use libfuzzer_sys::fuzz_target;
use microscope::sanitize::{
    Action, Builtin, FieldClass, HashKey, Outcome, Rule, RuleSpec, Sanitizer, Tally,
};

const ACTIONS: [Action; 5] = [
    Action::Redact,
    Action::Hash,
    Action::Mask,
    Action::DropField,
    Action::DropRecord,
];

fuzz_target!(|input: &[u8]| {
    let Some((&choice, rest)) = input.split_first() else {
        return;
    };
    let (pattern, value) = match rest.iter().position(|&byte| byte == b'\n') {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (&[][..], rest),
    };

    let custom = Rule::new(&RuleSpec {
        name: "fuzzed".to_owned(),
        fields: FieldClass::ALL
            .iter()
            .map(|class| class.name().to_owned())
            .collect(),
        pattern: String::from_utf8_lossy(pattern).into_owned(),
        action: ACTIONS[usize::from(choice) % ACTIONS.len()],
    });
    let sanitizer = Sanitizer::new(
        &Builtin::ALL,
        custom.into_iter().collect(),
        HashKey::new(b"fuzz"),
    );

    let mut tally = Tally::default();
    let class = FieldClass::ALL[usize::from(choice >> 4) % FieldClass::ALL.len()];
    let _ = sanitizer.bytes(class, value, &mut tally);
    let lossy = String::from_utf8_lossy(value);
    let _ = sanitizer.text(class, &lossy, &mut tally);
    assert!(sanitizer.fired(&tally).count() == sanitizer.rules().len());

    let args: Vec<String> = lossy.split('\t').map(str::to_owned).collect();
    let mut arg_tally = Tally::default();
    if let Outcome::Replaced(cleaned) = sanitizer.args(&args, &mut arg_tally) {
        assert_eq!(cleaned.len(), args.len());
    }
});
