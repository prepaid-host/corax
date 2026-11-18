//! Check scheduling: a min-heap of due times with lazy invalidation.
//!
//! Each [`CheckState`] carries a `sched_generation`. Rescheduling bumps it and pushes a
//! new heap entry; outdated entries are skipped when popped. This keeps rescheduling
//! O(log n) without a decrease-key operation.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::hash::{Hash, Hasher};

use corax_config::timeperiod;

use crate::model::*;
use crate::World;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    due_ms: i64,
    target: Target,
    generation: u32,
}

#[derive(Debug, Default)]
pub struct Scheduler {
    heap: BinaryHeap<Reverse<Entry>>,
}

/// A due check popped from the queue.
#[derive(Debug, Clone, Copy)]
pub struct Due {
    pub target: Target,
    pub generation: u32,
    pub due_ms: i64,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Schedule `target` at unix time `at_ms` (milliseconds).
    pub fn schedule_ms(&mut self, w: &mut World, target: Target, at_ms: i64) {
        let st = w.state_mut(target);
        st.sched_generation = st.sched_generation.wrapping_add(1);
        st.next_check = at_ms.div_euclid(1000);
        self.heap.push(Reverse(Entry { due_ms: at_ms, target, generation: st.sched_generation }));
    }

    pub fn schedule(&mut self, w: &mut World, target: Target, at: i64) {
        self.schedule_ms(w, target, at * 1000);
    }

    /// Remove a target from the schedule (e.g. active checks disabled).
    pub fn unschedule(&mut self, w: &mut World, target: Target) {
        let st = w.state_mut(target);
        st.sched_generation = st.sched_generation.wrapping_add(1);
        st.next_check = 0;
    }

    /// Due time of the earliest (possibly stale) entry.
    pub fn next_due_ms(&self) -> Option<i64> {
        self.heap.peek().map(|Reverse(e)| e.due_ms)
    }

    /// Pop the next valid entry that is due at `now_ms`.
    pub fn pop_due(&mut self, w: &World, now_ms: i64) -> Option<Due> {
        while let Some(Reverse(e)) = self.heap.peek().copied() {
            if e.due_ms > now_ms {
                return None;
            }
            self.heap.pop();
            if w.state(e.target).sched_generation == e.generation {
                return Some(Due { target: e.target, generation: e.generation, due_ms: e.due_ms });
            }
        }
        None
    }

    /// Drop stale entries if the heap grew far beyond the number of objects.
    pub fn compact(&mut self, w: &World) {
        if self.heap.len() > 4 * (w.hosts.len() + w.services.len()) + 1024 {
            let entries: Vec<_> =
                self.heap.drain().filter(|Reverse(e)| w.state(e.target).sched_generation == e.generation).collect();
            self.heap = entries.into_iter().collect();
        }
    }
}

/// Whether an object should be actively checked by the scheduler at all.
pub fn wants_active_checks(w: &World, t: Target) -> bool {
    let has_cmd = w.common(t).check_command.is_some();
    let st = w.state(t);
    has_cmd && st.active_checks_enabled
}

/// Stable pseudo random offset in `[0, span)` milliseconds for initial check spreading.
fn spread_offset_ms(w: &World, t: Target, span_ms: i64) -> i64 {
    if span_ms <= 0 {
        return 0;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    w.target_name(t).hash(&mut h);
    (h.finish() % span_ms as u64) as i64
}

/// Initial scheduling after start/reload.
pub fn schedule_initial(w: &mut World, sched: &mut Scheduler, now: i64, use_retained: bool) {
    let targets: Vec<Target> = (0..w.hosts.len() as u32)
        .map(|i| Target::Host(HostId(i)))
        .chain((0..w.services.len() as u32).map(|i| Target::Service(ServiceId(i))))
        .collect();
    for t in targets {
        if !wants_active_checks(w, t) {
            continue;
        }
        let interval = w.check_interval_secs(t).max(1);
        let retained = w.state(t).next_check;
        if use_retained && retained > now && retained <= now + interval {
            sched.schedule(w, t, retained);
            continue;
        }
        let max_spread = match t {
            Target::Host(_) => w.settings.max_host_check_spread,
            Target::Service(_) => w.settings.max_service_check_spread,
        } * w.settings.interval_length;
        let span = interval.min(max_spread.max(1)) * 1000;
        let at = now * 1000 + spread_offset_ms(w, t, span);
        sched.schedule_ms(w, t, at);
    }
}

/// Next regular check time after a check that started at `last_start`.
pub fn next_regular(w: &World, t: Target, last_start: i64, now: i64) -> i64 {
    let st = w.state(t);
    let interval = if st.current_state != 0 && st.state_type == StateType::Soft {
        w.retry_interval_secs(t)
    } else {
        w.check_interval_secs(t)
    }
    .max(1);
    let mut next = last_start + interval;
    if next <= now {
        // We are late (e.g. long running check): skip forward, keep the phase.
        next = now + interval - (now - last_start).rem_euclid(interval);
    }
    let period = w.common(t).check_period.as_deref();
    timeperiod::next_active(&w.objects.timeperiods, period, next).unwrap_or(next + 86400)
}

/// Reschedule a target after a check (only if active checks are wanted).
pub fn reschedule_after_check(w: &mut World, sched: &mut Scheduler, t: Target, last_start: i64, now: i64) {
    if !wants_active_checks(w, t) {
        return;
    }
    let next = next_regular(w, t, last_start, now);
    sched.schedule(w, t, next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{world_from, OBJ};

    #[test]
    fn heap_order_and_invalidation() {
        let mut w = world_from(OBJ);
        let mut s = Scheduler::new();
        let h = Target::Host(HostId(0));
        let svc = Target::Service(ServiceId(0));
        s.schedule(&mut w, h, 100);
        s.schedule(&mut w, svc, 50);
        s.schedule(&mut w, h, 10); // reschedule earlier, old entry becomes stale
        assert_eq!(s.pop_due(&w, 1_000_000).unwrap().target, h);
        assert_eq!(s.pop_due(&w, 1_000_000).unwrap().target, svc);
        assert!(s.pop_due(&w, 1_000_000).is_none());
    }

    #[test]
    fn initial_spread_within_interval() {
        let mut w = world_from(OBJ);
        let mut s = Scheduler::new();
        schedule_initial(&mut w, &mut s, 1000, false);
        for t in [Target::Host(HostId(0)), Target::Service(ServiceId(0))] {
            let nc = w.state(t).next_check;
            assert!((1000..1000 + 300).contains(&nc), "{nc}");
        }
    }
}
