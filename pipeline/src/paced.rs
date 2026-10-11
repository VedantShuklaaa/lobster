//! Latency under a controlled load.
//!
//! The caller sends commands on a fixed schedule (say 5 million per second)
//! and each command's latency is
//!
//! ```text
//! (when the engine finished it) - (when it was DUE to be sent)
//! ```
//!
//! "Finished" means the engine has applied the command and published its
//! events. Measuring from the due time, not from the moment the caller got
//! around to sending, matters: if the system falls behind, the commands stuck
//! waiting are charged for the waiting. Measuring from the actual send would
//! hide exactly the delay we are looking for (coordinated omission).
//!
//! Commands that produce no events cannot be seen on the event ring, so the
//! engine thread timestamps each command itself. Both threads read the same
//! monotonic clock (`Instant`), so the two timestamps are comparable.
//!
//! Paced runs always publish every item on both rings (batch 1): batching
//! trades latency for throughput and has no place in a latency measurement.

use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use engine::{Command, Event, OrderBook};

use crate::{Config, FNV_OFFSET, Summary, drain, pin, process, spsc};

/// First command is due this long after the clock starts, so the engine thread
/// is running (and pinned) before the first one arrives.
const START_DELAY_NS: u64 = 5_000_000;

pub struct Paced {
    pub summary: Summary,
    /// Per command, nanoseconds since the run's clock started.
    pub due_ns: Vec<u64>,
    pub done_ns: Vec<u64>,
}

impl Paced {
    /// Sorted latencies in ns, skipping the first `skip_frac` of the commands
    /// (warm-up: cold caches, the first page faults).
    pub fn latencies(&self, skip_frac: f64) -> Vec<u64> {
        let skip = (self.due_ns.len() as f64 * skip_frac) as usize;
        let mut v: Vec<u64> = self.due_ns[skip..]
            .iter()
            .zip(&self.done_ns[skip..])
            .map(|(&due, &done)| done.saturating_sub(due))
            .collect();
        v.sort_unstable();
        v
    }

    /// Commands per second actually completed, from the first due time to the
    /// last completion. Falls below the offered rate when the system saturates.
    pub fn achieved_rate(&self) -> f64 {
        let (Some(&first), Some(&last)) = (self.due_ns.first(), self.done_ns.last()) else {
            return 0.0;
        };
        self.due_ns.len() as f64 / (last.saturating_sub(first)).max(1) as f64 * 1e9
    }
}

#[inline]
fn now_ns(base: &Instant) -> u64 {
    base.elapsed().as_nanos() as u64
}

/// Sends `cmds` at `rate_per_sec` and records both timestamps per command.
pub fn run_paced(cmds: &[Command], cfg: Config, rate_per_sec: f64) -> Paced {
    assert!(rate_per_sec > 0.0, "rate must be positive");
    let n = cmds.len();
    let period_ns = 1e9 / rate_per_sec;

    let (mut cmd_tx, mut cmd_rx) = spsc::channel::<Command>(cfg.cmd_ring);
    let (mut evt_tx, mut evt_rx) = spsc::channel::<Event>(cfg.evt_ring);
    let cmds_done = AtomicBool::new(false);
    let engine_done = AtomicBool::new(false);
    let (cmds_done, engine_done) = (&cmds_done, &engine_done);

    let mut events = 0u64;
    let mut hash = FNV_OFFSET;
    let mut due_ns = Vec::with_capacity(n);

    if let Some(cpu) = cfg.caller_cpu {
        let _ = pin::pin_current_thread(cpu);
    }
    let base = Instant::now();
    let base = &base;

    let done_ns = thread::scope(|s| {
        let engine = s.spawn(move || {
            if let Some(cpu) = cfg.engine_cpu {
                let _ = pin::pin_current_thread(cpu);
            }
            let mut book = OrderBook::with_capacity(cfg.index_capacity);
            let mut out = Vec::with_capacity(64);
            let mut done = Vec::with_capacity(n);
            let mut evt_full_spins = 0u64;
            loop {
                if let Some(c) = cmd_rx.pop() {
                    process(&mut book, &mut out, &mut evt_tx, &mut evt_full_spins, c);
                    done.push(now_ns(base));
                } else if cmds_done.load(Ordering::Acquire) {
                    while let Some(c) = cmd_rx.pop() {
                        process(&mut book, &mut out, &mut evt_tx, &mut evt_full_spins, c);
                        done.push(now_ns(base));
                    }
                    break;
                } else {
                    spin_loop();
                }
            }
            engine_done.store(true, Ordering::Release);
            done
        });

        for (i, &c) in cmds.iter().enumerate() {
            let due = START_DELAY_NS + (i as f64 * period_ns) as u64;
            // idle until the command is due; use the time to drain events
            while now_ns(base) < due {
                drain(&mut evt_rx, &mut hash, &mut events);
                spin_loop();
            }
            due_ns.push(due);
            let mut c = c;
            while let Err(back) = cmd_tx.push(c) {
                c = back;
                drain(&mut evt_rx, &mut hash, &mut events);
                spin_loop();
            }
        }
        cmds_done.store(true, Ordering::Release);

        loop {
            drain(&mut evt_rx, &mut hash, &mut events);
            if engine_done.load(Ordering::Acquire) {
                drain(&mut evt_rx, &mut hash, &mut events);
                break;
            }
            spin_loop();
        }
        engine.join().expect("engine thread panicked")
    });

    Paced {
        summary: Summary {
            commands: n as u64,
            events,
            hash,
        },
        due_ns,
        done_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_direct;
    use generator::{GenConfig, generate};

    fn stream(n: usize) -> Vec<Command> {
        generate(&GenConfig::narrow()).into_iter().take(n).collect()
    }

    #[test]
    fn paced_run_matches_direct_and_timestamps_are_sane() {
        let cmds = stream(20_000);
        let p = run_paced(&cmds, Config::default(), 2_000_000.0);
        assert_eq!(
            p.summary,
            run_direct(&cmds, Config::default().index_capacity)
        );
        assert_eq!(p.due_ns.len(), cmds.len());
        assert_eq!(p.done_ns.len(), cmds.len());
        assert!(
            p.due_ns.windows(2).all(|w| w[0] < w[1]),
            "due times increase"
        );
        assert!(
            p.done_ns.windows(2).all(|w| w[0] <= w[1]),
            "FIFO completion"
        );
        // nothing finishes before it was due
        assert!(p.due_ns.iter().zip(&p.done_ns).all(|(d, f)| f >= d));
        let lat = p.latencies(0.1);
        assert_eq!(lat.len(), cmds.len() - cmds.len() / 10);
        assert!(lat.windows(2).all(|w| w[0] <= w[1]), "sorted");
    }

    #[test]
    fn tiny_rings_do_not_deadlock_when_paced() {
        let cmds = stream(10_000);
        let cfg = Config {
            cmd_ring: 2,
            evt_ring: 2,
            ..Config::default()
        };
        let p = run_paced(&cmds, cfg, 10_000_000.0);
        assert_eq!(p.summary, run_direct(&cmds, cfg.index_capacity));
    }

    #[test]
    fn overload_is_charged_to_latency() {
        // far above what one engine thread can take: the schedule falls behind
        // and the later commands must show the backlog, not hide it
        let cmds = stream(50_000);
        let p = run_paced(&cmds, Config::default(), 1e9);
        let lat = p.latencies(0.0);
        assert!(lat[lat.len() - 1] > 10 * lat[0].max(1) || lat[lat.len() - 1] > 100_000);
    }
}
