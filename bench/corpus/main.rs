use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;

#[derive(Clone)]
struct Record {
    name: String,
    samples: Vec<i32>,
    weight: Option<f64>,
}

enum Label {
    Count(usize),
    Unit(String),
}

fn main() {
    let records = vec![
        Record {
            name: "alpha".into(),
            samples: vec![2, 3, 5],
            weight: Some(1.5),
        },
        Record {
            name: "beta".into(),
            samples: vec![7, 11],
            weight: None,
        },
    ];
    let labels = BTreeMap::from([
        ("count", Label::Count(records.len())),
        ("unit", Label::Unit("samples".into())),
    ]);
    let lookup: HashMap<_, _> = records
        .iter()
        .map(|r| (r.name.clone(), r.clone()))
        .collect();
    let selected = Box::new(records[0].clone());
    let result: Result<&Record, &str> = Ok(&selected);
    for label in labels.values() {
        match label {
            Label::Count(count) => {
                black_box(count);
            }
            Label::Unit(unit) => {
                black_box(unit);
            }
        }
    }
    black_box((&records, &lookup, &selected, &result));
    black_box((&selected.samples, selected.weight));
}
