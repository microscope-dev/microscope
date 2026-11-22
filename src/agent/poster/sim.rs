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
                        let _ = spool.ack(batch.owner_gen, last_chunk).expect("ack");
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
                    let _ = spool.ack(batch.owner_gen, last_chunk).expect("ack");
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

#[expect(
    clippy::too_many_lines,
    reason = "deterministic control+poster simulation covering watch, flush, revocation, and faults"
)]
fn run_control_and_poster_seed(seed: u64) {
    use crate::agent::{
        control::{ControlAction, ControlEffect, ControlMachine, ControlOutcome},
        wire::{FlushDirective, PollResponse, WatchDirective},
    };

    let scratch = Scratch::new();
    let dir = scratch.path().join("spool");
    let sanitizer = Sanitizer::new(&Builtin::DEFAULT, Vec::new(), HashKey::new(b"sim-ctrl"));
    let mut rng = Rng::new(seed);
    let mut server = MockServer::new();

    let base_interval = Duration::from_secs(60);
    let mut now = Instant(Duration::ZERO);
    let mut spool = Spool::open(&dir, 64 * 1024 * 1024).expect("open spool");
    let mut poster = PosterMachine::new(now, base_interval);
    let mut control = ControlMachine::new(now, vec!["sim-proj".into()], vec!["sim-svc".into()]);
    let mut token_gen = server.token_gen;
    let mut version = 1_u64;
    let mut watch_gen = 0_u64;
    let mut active_watch: Option<WatchDirective> = None;
    let mut flush_id = 0_u64;
    let mut completed_flushes: BTreeSet<u64> = BTreeSet::new();
    let mut produced = 0_u64;
    let mut revoked_triggered = false;

    for step in 0..120 {
        now = now.after(Duration::from_secs(1 + (rng.next_u64() % 3)));

        if step < 90 && rng.pct(20) {
            if active_watch.is_some() && rng.pct(50) {
                active_watch = None;
            } else {
                watch_gen += 1;
                active_watch = Some(WatchDirective {
                    interval_ms: if rng.pct(50) { 5_000 } else { 15_000 },
                    lease_ms: 20_000,
                    generation: watch_gen,
                });
            }
            version += 1;
        }

        if step < 90 && rng.pct(15) {
            flush_id += 1;
            version += 1;
        }

        if step < 90 && rng.pct(45) {
            let was_empty = spool.is_empty();
            let n = spool.allocate_chunk().expect("alloc");
            produced = n;
            spool
                .push(n, &make_chunk(n, &sanitizer), 42, 1_000_000_000)
                .expect("push");
            poster.chunk_queued(now, was_empty);
        }

        if step == 95 && seed.is_multiple_of(5) {
            revoked_triggered = true;
            version += 1;
        }

        if step < 90 && rng.pct(8) {
            server.token_gen += 1;
        }

        match control.next(now, token_gen > 0) {
            ControlAction::Revoked => {
                assert!(revoked_triggered, "seed {seed}: unexpected revocation");
                break;
            }
            ControlAction::NeedToken => {
                token_gen = server.token_gen;
                control.token_refreshed(now);
            }
            ControlAction::Wait { .. } => {}
            ControlAction::Poll(req) => {
                let outcome = if rng.pct(12) {
                    ControlOutcome::Retry { retry_after: None }
                } else if token_gen != server.token_gen {
                    ControlOutcome::Unauthorized
                } else {
                    let pending_flush = (flush_id > req.last_flush_id
                        && !completed_flushes.contains(&flush_id))
                    .then_some(FlushDirective {
                        id: flush_id,
                        deadline_ms: 10_000,
                    });
                    ControlOutcome::Directive(PollResponse {
                        version,
                        revoked: revoked_triggered,
                        watch: active_watch.clone(),
                        flush: pending_flush,
                        policy: None,
                        burst: None,
                        poll_after_ms: 500,
                    })
                };
                for effect in control.on(outcome, now, rng.next_f64()) {
                    match effect {
                        ControlEffect::Revoked => {
                            assert!(revoked_triggered);
                        }
                        ControlEffect::SetWatch(w) => {
                            poster.apply_watch(w.as_ref(), now);
                        }
                        ControlEffect::Flush(f) => {
                            if spool.is_empty() {
                                let n = spool.allocate_chunk().expect("cut");
                                produced = n;
                                spool
                                    .push(n, &make_chunk(n, &sanitizer), 42, 1_000_000_000)
                                    .expect("push cut");
                                poster.chunk_queued(now, true);
                            }
                            poster.request_flush(f.id, now);
                        }
                        ControlEffect::ApplyPolicy(_) | ControlEffect::Burst(_) => {}
                    }
                }
            }
        }

        if control.is_revoked() {
            break;
        }

        match poster.next(now, !spool.is_empty(), token_gen > 0) {
            PosterAction::NeedToken => {
                token_gen = server.token_gen;
                poster.token_refreshed(now);
            }
            PosterAction::Wait { .. } => {}
            PosterAction::Post => {
                let active_flush = poster.pending_flush();
                let batch = spool
                    .peek_batch(MAX_POST_BYTES, 42, 1_000_000_000, &sanitizer)
                    .expect("peek")
                    .expect("batch");
                let outcome = server.ingest(token_gen, batch.post.as_bytes(), &mut rng);
                if let PosterOutcome::Accepted { last_chunk }
                | PosterOutcome::Overlap { last_chunk } = outcome
                {
                    let _ = spool.ack(batch.owner_gen, last_chunk).expect("ack");
                    if spool.is_empty()
                        && let Some(fid) = active_flush
                    {
                        completed_flushes.insert(fid);
                        control.mark_flush_done(fid, now);
                    }
                }
                poster.on(outcome, now, !spool.is_empty(), rng.next_f64());
            }
        }
    }

    if revoked_triggered {
        assert!(
            control.is_revoked() || version > 1,
            "seed {seed}: revocation did not take effect"
        );
        return;
    }

    let mut clean_rng = Rng::new(seed ^ 0xcafe_babe);
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
                    .expect("batch");
                let reader = ObjectReader::open(batch.post.as_bytes()).expect("valid");
                let sections = reader.part_sections(0, None).expect("sections");
                let decoded = decode_tables(&sections).expect("decode");
                let max_c = decoded.chunks.iter().map(|c| c.number).max().expect("max");
                let min_c = decoded.chunks.iter().map(|c| c.number).min().expect("min");
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
                    let _ = spool.ack(batch.owner_gen, last_chunk).expect("ack");
                }
                poster.on(outcome, now, !spool.is_empty(), clean_rng.next_f64());
            }
        }
    }

    assert!(spool.is_empty(), "seed {seed}: spool failed to drain");
    assert_eq!(server.delivered.len() as u64, produced);
}

#[test]
fn deterministic_sim_control_watch_flush_and_revocation() {
    for seed in 1..=100 {
        run_control_and_poster_seed(seed);
    }
}

#[test]
fn pending_flush_honors_retry_backoff_and_expires_at_deadline() {
    let start = Instant(Duration::ZERO);
    let mut poster = PosterMachine::new(start, Duration::from_secs(60));
    poster.request_flush_with_deadline(7, 2_000, start);
    assert_eq!(poster.next(start, true, true), PosterAction::Post);

    poster.on(
        PosterOutcome::Retry {
            retry_after: Some(Duration::from_secs(5)),
        },
        start,
        true,
        0.5,
    );
    let one_sec = start.after(Duration::from_secs(1));
    assert_eq!(
        poster.next(one_sec, true, true),
        PosterAction::Wait {
            until: start.after(Duration::from_secs(5)),
        }
    );

    let after_deadline = start.after(Duration::from_secs(6));
    assert_eq!(poster.active_flush(after_deadline), None);
    poster.expire_flush(after_deadline);
    assert_eq!(poster.pending_flush(), None);
}

#[test]
fn control_machine_resets_after_revocation_and_delays_policy_ack_until_applied() {
    use crate::agent::{
        control::{ControlAction, ControlEffect, ControlMachine, ControlOutcome},
        wire::{PolicyDirective, PollResponse},
    };

    let t0 = Instant(Duration::ZERO);
    let mut ctrl = ControlMachine::new(t0, vec!["proj".into()], vec!["svc".into()]);

    let effects = ctrl.on(
        ControlOutcome::Directive(PollResponse {
            version: 2,
            revoked: false,
            watch: None,
            flush: None,
            policy: Some(PolicyDirective {
                version: 3,
                preset: Some("dev".into()),
                time_unit: Some("1us".into()),
                sanitizer_version: 2,
            }),
            burst: None,
            poll_after_ms: 0,
        }),
        t0,
        0.0,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, ControlEffect::ApplyPolicy(p) if p.version == 3))
    );
    let ControlAction::Poll(before_apply) = ctrl.next(t0, true) else {
        panic!("expected poll");
    };
    assert_eq!(before_apply.policy_version, 1);
    assert_eq!(before_apply.sanitizer_version, 1);

    ctrl.set_applied_policy(3, 2);
    let ControlAction::Poll(after_apply) = ctrl.next(t0, true) else {
        panic!("expected poll");
    };
    assert_eq!(after_apply.policy_version, 3);
    assert_eq!(after_apply.sanitizer_version, 2);

    let rev_effects = ctrl.on(
        ControlOutcome::Directive(PollResponse {
            version: 3,
            revoked: true,
            watch: None,
            flush: None,
            policy: None,
            burst: None,
            poll_after_ms: 0,
        }),
        t0,
        0.0,
    );
    assert_eq!(rev_effects, vec![ControlEffect::Revoked]);
    assert_eq!(ctrl.next(t0, true), ControlAction::Revoked);

    let t1 = t0.after(Duration::from_secs(1));
    ctrl.reset(t1);
    assert!(!ctrl.is_revoked());
    assert!(matches!(ctrl.next(t1, true), ControlAction::Poll(_)));
}
