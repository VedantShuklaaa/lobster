//! Engine thread behind SPSC rings.
//!
//! ```text
//!   caller thread                         engine thread
//!   -------------   cmd ring (SPSC)   --------------------
//!   push Command  ------------------>  pop, apply_into
//!   pop Event     <------------------  push each Event      evt ring (SPSC)
//! ```
//!
//! The rings carry the engine's own `Command` and `Event` types unchanged.
//! The matching engine itself stays single-threaded and unaware of any of this.

pub mod spsc;

use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use engine::{Command, Event, OrderBook};

/// What a run produced. Two runs are equivalent when all three fields match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub commands: u64,
    pub events: u64,
    /// Order-sensitive hash over the whole event stream.
    pub hash: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Passed to `OrderBook::with_capacity`.
    pub index_capacity: usize,
    pub cmd_ring: usize,
    pub evt_ring: usize,
    /// Each ring side publishes its counter every `batch` items. 1 = every
    /// item (lowest latency); larger = fewer cache-line handoffs.
    pub batch: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            index_capacity: 1 << 16,
            cmd_ring: 1 << 12,
            evt_ring: 1 << 12,
            batch: 1,
        }
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

#[inline]
fn mix(h: u64, x: u64) -> u64 {
    (h ^ x).wrapping_mul(0x0000_0100_0000_01b3)
}

#[inline]
fn hash_event(h: u64, e: &Event) -> u64 {
    match *e {
        Event::Fill(f) => {
            let h = mix(h, 0);
            let h = mix(h, f.maker);
            let h = mix(h, f.taker);
            let h = mix(h, f.price as u64);
            mix(h, f.qty as u64)
        }
        Event::Rejected { id, reason } => mix(mix(mix(h, 1), id), reason as u64),
    }
}

/// Baseline: the engine on the calling thread, no rings.
pub fn run_direct(cmds: &[Command], index_capacity: usize) -> Summary {
    let mut book = OrderBook::with_capacity(index_capacity);
    let mut out = Vec::with_capacity(64);
    let mut events = 0u64;
    let mut hash = FNV_OFFSET;
    for &c in cmds {
        out.clear();
        book.apply_into(c, &mut out);
        for e in &out {
            hash = hash_event(hash, e);
        }
        events += out.len() as u64;
    }
    Summary {
        commands: cmds.len() as u64,
        events,
        hash,
    }
}

fn drain(rx: &mut spsc::Consumer<Event>, hash: &mut u64, events: &mut u64) {
    while let Some(e) = rx.pop() {
        *hash = hash_event(*hash, &e);
        *events += 1;
    }
}

/// Where each side had to wait. Plain counters bumped only inside the waiting
/// branches, so they cost nothing on the path where nobody waits.
///
/// How to read them:
/// - `engine_idle_polls` large, `caller_cmd_full_spins` near 0: the engine keeps
///   running dry, so the caller (push, drain, hash) is the limit.
/// - `caller_cmd_full_spins` large, `engine_idle_polls` near 0: the command ring
///   keeps filling, so the engine thread is the limit.
/// - `engine_evt_full_spins` large: the caller is not draining events fast enough.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub engine_idle_polls: u64,
    pub engine_evt_full_spins: u64,
    pub caller_cmd_full_spins: u64,
}

/// Runs one command and pushes its events, waiting while the ring is full.
fn process(
    book: &mut OrderBook,
    out: &mut Vec<Event>,
    evt_tx: &mut spsc::Producer<Event>,
    evt_full_spins: &mut u64,
    c: Command,
) {
    out.clear();
    book.apply_into(c, out);
    for &e in out.iter() {
        let mut e = e;
        // event ring full: wait for the caller to drain it
        while let Err(back) = evt_tx.push(e) {
            e = back;
            *evt_full_spins += 1;
            spin_loop();
        }
    }
}

/// The same work, with the engine on its own thread.
pub fn run_pipeline(cmds: &[Command], cfg: Config) -> Summary {
    run_pipeline_stats(cmds, cfg).0
}

/// `run_pipeline`, plus the wait counters.
pub fn run_pipeline_stats(cmds: &[Command], cfg: Config) -> (Summary, Stats) {
    let (mut cmd_tx, mut cmd_rx) = spsc::channel_with_batch::<Command>(cfg.cmd_ring, cfg.batch);
    let (mut evt_tx, mut evt_rx) = spsc::channel_with_batch::<Event>(cfg.evt_ring, cfg.batch);

    // "no more commands will arrive" / "the engine has pushed its last event".
    // Each is set AFTER the last push it describes, with Release, and read with
    // Acquire, so seeing the flag means seeing everything before it.
    let cmds_done = AtomicBool::new(false);
    let engine_done = AtomicBool::new(false);
    let (cmds_done, engine_done) = (&cmds_done, &engine_done);

    let mut events = 0u64;
    let mut hash = FNV_OFFSET;

    let mut stats = Stats::default();

    thread::scope(|s| {
        let engine = s.spawn(move || {
            let mut book = OrderBook::with_capacity(cfg.index_capacity);
            let mut out = Vec::with_capacity(64);
            let (mut idle_polls, mut evt_full_spins) = (0u64, 0u64);
            loop {
                if let Some(c) = cmd_rx.pop() {
                    process(&mut book, &mut out, &mut evt_tx, &mut evt_full_spins, c);
                } else if cmds_done.load(Ordering::Acquire) {
                    // flag seen, so every command is visible: drain the rest
                    while let Some(c) = cmd_rx.pop() {
                        process(&mut book, &mut out, &mut evt_tx, &mut evt_full_spins, c);
                    }
                    break;
                } else {
                    // idle: don't sit on events the caller may be waiting for
                    idle_polls += 1;
                    evt_tx.publish();
                    spin_loop();
                }
            }
            evt_tx.publish();
            engine_done.store(true, Ordering::Release);
            (idle_polls, evt_full_spins)
        });

        for (i, &c) in cmds.iter().enumerate() {
            let mut c = c;
            while let Err(back) = cmd_tx.push(c) {
                c = back;
                // Command ring full. Drain events while we wait; if we only
                // waited, a full event ring would stall the engine, and the
                // two of us would wait on each other forever.
                stats.caller_cmd_full_spins += 1;
                drain(&mut evt_rx, &mut hash, &mut events);
                spin_loop();
            }
            if i & 255 == 0 {
                drain(&mut evt_rx, &mut hash, &mut events);
            }
        }
        cmd_tx.publish();
        cmds_done.store(true, Ordering::Release);

        loop {
            drain(&mut evt_rx, &mut hash, &mut events);
            if engine_done.load(Ordering::Acquire) {
                drain(&mut evt_rx, &mut hash, &mut events);
                break;
            }
            spin_loop();
        }

        let (idle_polls, evt_full_spins) = engine.join().expect("engine thread panicked");
        stats.engine_idle_polls = idle_polls;
        stats.engine_evt_full_spins = evt_full_spins;
    });

    (
        Summary {
            commands: cmds.len() as u64,
            events,
            hash,
        },
        stats,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use generator::{GenConfig, generate};

    fn presets() -> Vec<(&'static str, GenConfig)> {
        vec![
            ("narrow", GenConfig::narrow()),
            ("bursty", GenConfig::bursty()),
            ("wide", GenConfig::wide()),
            ("sparse", GenConfig::sparse()),
        ]
    }

    #[test]
    fn pipeline_matches_direct_on_every_preset() {
        for (name, cfg) in presets() {
            let cmds: Vec<Command> = generate(&cfg).into_iter().take(150_000).collect();
            let direct = run_direct(&cmds, 1 << 16);
            for (cmd_ring, evt_ring, batch) in [
                (1 << 12, 1 << 12, 1),
                (16, 16, 1),
                (2, 2, 1),
                (4096, 2, 1),
                (1 << 12, 1 << 12, 64),
                (16, 16, 64),
                (2, 2, 64),
                (4096, 2, 256),
            ] {
                let got = run_pipeline(
                    &cmds,
                    Config {
                        index_capacity: 1 << 16,
                        cmd_ring,
                        evt_ring,
                        batch,
                    },
                );
                assert_eq!(
                    got, direct,
                    "{name} with rings {cmd_ring}/{evt_ring} batch {batch}"
                );
            }
            assert!(direct.events > 0, "{name} produced no events");
        }
    }

    #[test]
    fn empty_stream() {
        let got = run_pipeline(&[], Config::default());
        assert_eq!(got, run_direct(&[], 16));
        assert_eq!(got.events, 0);
    }

    #[test]
    fn hash_is_order_and_content_sensitive() {
        use engine::{Fill, RejectReason};
        let f = |maker, qty| {
            Event::Fill(Fill {
                maker,
                taker: 9,
                price: 100,
                qty,
            })
        };
        let r = Event::Rejected {
            id: 7,
            reason: RejectReason::NoLiquidity,
        };
        let h = |evs: &[Event]| evs.iter().fold(FNV_OFFSET, hash_event);
        assert_ne!(h(&[f(1, 5), r]), h(&[r, f(1, 5)])); // order
        assert_ne!(h(&[f(1, 5)]), h(&[f(1, 6)])); // a field
        assert_ne!(h(&[f(1, 5)]), h(&[f(2, 5)])); // another field
        assert_ne!(h(&[r]), h(&[])); // presence
    }

    #[test]
    fn dropping_a_command_changes_the_summary() {
        let cmds: Vec<Command> = generate(&GenConfig::narrow())
            .into_iter()
            .take(50_000)
            .collect();
        let full = run_direct(&cmds, 1 << 16);
        let mut short = cmds.clone();
        short.remove(25_000);
        assert_ne!(run_direct(&short, 1 << 16), full);
        assert_ne!(run_pipeline(&short, Config::default()), full);
    }

    #[test]
    fn stats_run_matches_plain_run_and_waits_show_up() {
        let cmds: Vec<Command> = generate(&GenConfig::narrow())
            .into_iter()
            .take(100_000)
            .collect();
        let cfg = Config {
            cmd_ring: 4,
            evt_ring: 4,
            ..Config::default()
        };
        let (sum, stats) = run_pipeline_stats(&cmds, cfg);
        assert_eq!(sum, run_direct(&cmds, cfg.index_capacity));
        // rings this small make somebody wait at least once
        assert!(
            stats.engine_idle_polls + stats.engine_evt_full_spins + stats.caller_cmd_full_spins > 0
        );
    }
}
