use std::{collections::BTreeSet, time::Duration};

use microscope_data::{
    capture::Capture,
    object::{ChunkData, ChunkRow, DimensionRow, ObjectReader, TimeUnit, decode_tables},
    sanitize::{Builtin, HashKey, Sanitizer, sanitize_chunk},
};

use super::{PosterAction, PosterMachine, PosterOutcome};
use crate::agent::{
    machine::Instant,
    scratch::Scratch,
    spool::{MAX_POST_BYTES, Spool},
};

struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    const fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        #[expect(clippy::cast_precision_loss, reason = "deterministic test jitter")]
        let v = (self.next_u64() >> 11) as f64 / ((1_u64 << 53) as f64);
        v
    }

    const fn pct(&mut self, threshold: u64) -> bool {
        (self.next_u64() % 100) < threshold
    }
}

struct MockServer {
    last_chunk: u64,
    delivered: Vec<u64>,
    delivered_set: BTreeSet<u64>,
    token_gen: u64,
}

impl MockServer {
    const fn new() -> Self {
        Self {
            last_chunk: 0,
            delivered: Vec::new(),
            delivered_set: BTreeSet::new(),
            token_gen: 1,
        }
    }

    fn ingest(&mut self, token_gen: u64, bytes: &[u8], rng: &mut Rng) -> PosterOutcome {
        if rng.pct(10) {
            return PosterOutcome::Retry { retry_after: None };
        }
        if rng.pct(8) {
            return PosterOutcome::Retry {
                retry_after: Some(Duration::from_secs(2 + (rng.next_u64() % 5))),
            };
        }
        if token_gen != self.token_gen {
            return PosterOutcome::Unauthorized;
        }

        let reader = ObjectReader::open(bytes).expect("valid USCOPEOB post");
        let sections = reader.part_sections(0, None).expect("sections");
        let decoded = decode_tables(&sections).expect("decode post");
        let min_chunk = decoded
            .chunks
            .iter()
            .map(|c| c.number)
            .min()
            .expect("non-empty batch");
        let max_chunk = decoded
            .chunks
            .iter()
            .map(|c| c.number)
            .max()
            .expect("non-empty batch");

        if max_chunk <= self.last_chunk {
            return PosterOutcome::Accepted {
                last_chunk: self.last_chunk,
            };
        }
        if min_chunk <= self.last_chunk {
            return PosterOutcome::Overlap {
                last_chunk: self.last_chunk,
            };
        }

        for row in &decoded.chunks {
            assert!(
                self.delivered_set.insert(row.number),
                "duplicate chunk {} delivered",
                row.number
            );
            self.delivered.push(row.number);
        }
        self.last_chunk = max_chunk;

        if rng.pct(15) {
            return PosterOutcome::Retry { retry_after: None };
        }

        PosterOutcome::Accepted {
            last_chunk: self.last_chunk,
        }
    }
}

fn make_chunk(number: u64, sanitizer: &Sanitizer) -> microscope_data::sanitize::SanitizedChunk {
    let data = ChunkData {
        capture: Capture::default(),
        chunks: vec![ChunkRow {
            agent: 42,
            number,
            start: i64::try_from(number * 5_000_000_000).expect("ts"),
            end: i64::try_from((number + 1) * 5_000_000_000).expect("ts"),
            sampled_ns: 5_000_000_000,
            preset: 0,
            time_unit: TimeUnit::Us,
            policy: 0,
            rules: "default".into(),
        }],
        dimensions: vec![
            DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "service.name".into(),
                value: "sim-svc".into(),
            },
            DimensionRow {
                process: microscope_data::capture::records::NONE,
                name: "microscope.project".into(),
                value: "sim-proj".into(),
            },
        ],
        sanitizer_counts: Vec::new(),
    };
    sanitize_chunk(data, TimeUnit::Us, sanitizer)
}

#[expect(
    clippy::too_many_lines,
    reason = "single deterministic simulation driver covering faults, crashes, and drain"
)]
fn run_seed(seed: u64, max_spool_bytes: u64) {
    let scratch = Scratch::new();
    let dir = scratch.path().join("spool");
    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"sim-key"));
    let mut rng = Rng::new(seed);
    let mut server = MockServer::new();

    let post_interval = Duration::from_secs(10);
    let mut now = Instant(Duration::ZERO);
    let mut spool = Spool::open(&dir, max_spool_bytes).expect("open spool");
    let mut poster = PosterMachine::new(now, post_interval);
    let mut token_gen = server.token_gen;
    let mut produced = 0_u64;
    let mut total_dropped = 0_u64;

    for step in 0..80 {
        now = now.after(Duration::from_secs(2 + (rng.next_u64() % 4)));

        if step < 40 && rng.pct(55) {
            let was_empty = spool.is_empty();
            let before_drop = spool.stats().dropped_chunks;
            let n = spool.allocate_chunk().expect("alloc");
            assert!(n > produced, "seed {seed}: non-monotonic chunk number {n}");
            produced = n;
            spool
                .push(n, &make_chunk(n, &sanitizer), 42, 1_000_000_000)
                .expect("push");
            total_dropped += spool.stats().dropped_chunks - before_drop;
            poster.chunk_queued(now, was_empty);
        }

        if step < 40 && rng.pct(8) {
            server.token_gen += 1;
        }

        if step < 40 && rng.pct(10) {
            drop(spool);
            spool = Spool::open(&dir, max_spool_bytes).expect("reopen after crash");
            poster = PosterMachine::new(now, post_interval);
        }

        let action = poster.next(now, !spool.is_empty(), token_gen > 0);
        match action {
            PosterAction::NeedToken => {
                token_gen = server.token_gen;
                poster.token_refreshed(now);
            }
            PosterAction::Wait { .. } => {}
            PosterAction::Post => {
                let batch = spool
                    .peek_batch(MAX_POST_BYTES, 42, 1_000_000_000, &sanitizer)
                    .expect("peek")
                    .expect("non-empty spool");
                let outcome = server.ingest(token_gen, batch.post.as_bytes(), &mut rng);
                match outcome {
                    PosterOutcome::Accepted { last_chunk }
                    | PosterOutcome::Overlap { last_chunk } => {
                        let _ = spool.ack(last_chunk).expect("ack");
                    }
                    PosterOutcome::Unauthorized | PosterOutcome::Retry { .. } => {}
                }
                poster.on(outcome, now, !spool.is_empty(), rng.next_f64());
            }
        }
    }

    let mut clean_rng = Rng::new(seed ^ 0xdead_beef);
    let mut drain_steps = 0;
    while !spool.is_empty() && drain_steps < 500 {
        drain_steps += 1;
        now = now.after(Duration::from_secs(60));
        match poster.next(now, !spool.is_empty(), token_gen > 0) {
            PosterAction::NeedToken => {
                token_gen = server.token_gen;
                poster.token_refreshed(now);
            }
            PosterAction::Wait { until } => {
                now = until;
            }
            PosterAction::Post => {
                let batch = spool
                    .peek_batch(MAX_POST_BYTES, 42, 1_000_000_000, &sanitizer)
                    .expect("peek")
                    .expect("non-empty spool");
                let reader = ObjectReader::open(batch.post.as_bytes()).expect("valid");
                let sections = reader.part_sections(0, None).expect("sections");
                let decoded = decode_tables(&sections).expect("decode");
                let min_c = decoded.chunks.iter().map(|c| c.number).min().expect("min");
                let max_c = decoded.chunks.iter().map(|c| c.number).max().expect("max");
                let outcome = if max_c <= server.last_chunk {
                    PosterOutcome::Accepted {
                        last_chunk: server.last_chunk,
                    }
                } else if min_c <= server.last_chunk {
                    PosterOutcome::Overlap {
                        last_chunk: server.last_chunk,
                    }
                } else {
                    for row in &decoded.chunks {
                        assert!(server.delivered_set.insert(row.number));
                        server.delivered.push(row.number);
                    }
                    server.last_chunk = max_c;
                    PosterOutcome::Accepted {
                        last_chunk: server.last_chunk,
                    }
                };
                if let PosterOutcome::Accepted { last_chunk }
                | PosterOutcome::Overlap { last_chunk } = outcome
                {
                    let _ = spool.ack(last_chunk).expect("ack");
                }
                poster.on(outcome, now, !spool.is_empty(), clean_rng.next_f64());
            }
        }
    }

    assert!(spool.is_empty(), "seed {seed}: spool failed to drain");
    assert!(
        server.delivered.windows(2).all(|w| w[0] < w[1]),
        "seed {seed}: delivered chunks not strictly increasing: {:?}",
        server.delivered
    );
    if max_spool_bytes >= 10 * 1024 * 1024 {
        assert_eq!(total_dropped, 0, "seed {seed}: unexpected spool drop");
        assert_eq!(
            server.delivered.len() as u64,
            produced,
            "seed {seed}: expected exact-once delivery of all {produced} chunks"
        );
    } else {
        assert!(
            server.delivered.len() as u64 + total_dropped >= produced,
            "seed {seed}: lost chunks beyond evicted count"
        );
    }
}

#[test]
fn deterministic_sim_exact_once_across_faults_and_crashes() {
    for seed in 1..=60 {
        run_seed(seed, 64 * 1024 * 1024);
    }
}

#[test]
fn deterministic_sim_spool_eviction_under_pressure() {
    for seed in 101..=130 {
        run_seed(seed, 1_500);
    }
}
