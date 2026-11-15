



use std::hint::black_box;

struct Node {
    name: &'static str,
    samples: u64,
    parent: usize,
}

struct Profile {
    nodes: Vec<Node>,
}


struct Folded(Vec<(String, u64)>);

const TREE: [(&str, u64, usize); 12] = [
    ("main", 0, 0),
    ("net/http.(*conn).serve", 2, 0),
    ("main.handleSearch", 3, 1),
    ("encoding/json.Unmarshal", 14, 2),
    ("runtime.mallocgc", 9, 3),
    ("main.(*Index).Query", 6, 2),
    ("sort.Slice", 11, 5),
    ("main.score", 17, 5),
    ("main.handleItems", 2, 1),
    ("database/sql.(*DB).Query", 4, 8),
    ("runtime.gcBgMarkWorker", 1, 0),
    ("runtime.scanobject", 13, 10),
];

fn sample(profile: &mut Profile, folded: &mut Folded, pass: u64) {
    for (index, node) in profile.nodes.iter_mut().enumerate() {
        if node.samples > 0 {
            node.samples += (index as u64 % 3 + pass) % 4;
        }
    }
    folded.0.clear();
    for index in 0..profile.nodes.len() {
        let mut stack = vec![profile.nodes[index].name];
        let mut at = index;
        while profile.nodes[at].parent != at {
            at = profile.nodes[at].parent;
            stack.push(profile.nodes[at].name);
        }
        stack.reverse();
        if profile.nodes[index].samples > 0 {
            folded.0.push((stack.join(";"), profile.nodes[index].samples));
        }
    }
}

fn main() {
    let mut profile = Profile {
        nodes: TREE
            .iter()
            .map(|&(name, samples, parent)| Node {
                name,
                samples,
                parent,
            })
            .collect(),
    };
    let mut folded = Folded(Vec::new());
    for pass in 1..=20 {
        sample(&mut profile, &mut folded, pass);
        let total: u64 = profile.nodes.iter().map(|node| node.samples).sum();
        println!("pass {pass}: {total} samples");
        black_box((&profile, &folded));
    }
}
