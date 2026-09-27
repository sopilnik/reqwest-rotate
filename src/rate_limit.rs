//! Per-host minimum request interval, enforced with `tokio::time`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Above this many distinct hosts, [`RateLimiter::wait`] prunes stale
/// entries, so a long-running process hitting an unbounded set of hosts
/// does not grow this map forever.
const PRUNE_ABOVE_HOSTS: usize = 1024;

/// Prunes are spaced at least this many `min_interval`s apart, capped at
/// [`MAX_PRUNE_SPACING`], so a steady set of many live hosts costs one scan
/// per spacing, not one per request. The map therefore holds at most the
/// hosts seen in the last `min_interval + spacing`.
const PRUNE_SPACING_MULTIPLE: u32 = 10;

/// Hard cap on the time between prunes: with a long `min_interval` (say a
/// minute per host) the multiple alone would let ten minutes' worth of
/// dead slots pile up before a scan.
const MAX_PRUNE_SPACING: Duration = Duration::from_secs(1);

/// A caller released no more than this before its earliest time counts as
/// on time. tokio rounds every sleep up to the next millisecond, so
/// sleeping out a shorter gap would land a millisecond late, and the next
/// caller, measured from that late release, would do the same: every
/// queued request would leave a millisecond after the one before.
const RELEASE_SLACK: Duration = Duration::from_millis(1);

/// Serialises requests to the same host so that no two requests to it start
/// less than `min_interval` apart. `None` (or a zero interval) disables
/// rate limiting entirely.
///
/// Not part of the public API: it is an implementation detail of
/// [`RotatingClient`](crate::RotatingClient), reachable only through its
/// `rate_limit` builder setting.
pub(crate) struct RateLimiter {
    min_interval: Option<Duration>,
    state: Mutex<State>,
}

struct State {
    /// Per host: the most recently reserved slot, and when a caller was
    /// last actually released for it.
    last: HashMap<String, (Instant, Option<Instant>)>,
    /// Earliest time the next prune may run, once one has happened.
    next_prune: Option<Instant>,
}

impl RateLimiter {
    pub(crate) fn new(min_interval: Option<Duration>) -> Self {
        Self {
            min_interval: min_interval
                .filter(|interval| !interval.is_zero())
                .map(|interval| interval.min(crate::MAX_DURATION)),
            state: Mutex::new(State {
                last: HashMap::new(),
                next_prune: None,
            }),
        }
    }

    /// Blocks the caller until it is that host's turn.
    ///
    /// Concurrent callers for the same host are queued in call order: each
    /// reserves the next free slot while holding the lock, before awaiting
    /// anything, so two callers never reserve the same slot. The lock is
    /// never held across an `.await`. If this call is dropped before its
    /// sleep completes (a `tokio::time::timeout`, say), it gives
    /// its slot back, unless another call has already queued behind it.
    /// After a stall that wakes several queued callers at once, they leave
    /// in the order the runtime runs them, still `min_interval` apart.
    pub(crate) async fn wait(&self, host: &str) {
        let Some(min_interval) = self.min_interval else {
            return;
        };
        let (mut guard, target, now) = {
            let mut state = self.lock();
            let now = Instant::now();
            state.maybe_prune(now, min_interval);

            let (target, previous) = match state.last.get_mut(host) {
                Some((slot, _)) => {
                    let previous = *slot;
                    let earliest_next = slot.checked_add(min_interval).unwrap_or(now);
                    let target = std::cmp::max(earliest_next, now);
                    *slot = target;
                    (target, Some(previous))
                }
                None => {
                    state.last.insert(host.to_owned(), (now, None));
                    (now, None)
                }
            };
            let guard = Reservation {
                limiter: self,
                host,
                target,
                previous,
                armed: true,
            };
            (guard, target, now)
        };
        if target > now {
            tokio::time::sleep_until(target).await;
        }
        guard.armed = false;

        // The reserved slot above spaces requests apart in theory; this
        // spaces them apart in practice too, in case a stalled runtime woke
        // every overdue waiter for this host in the same tick.
        loop {
            let wait_until = {
                let mut state = self.lock();
                let now = Instant::now();
                let Some((_, released)) = state.last.get_mut(host) else {
                    return;
                };
                match released.and_then(|r| r.checked_add(min_interval)) {
                    Some(earliest) if earliest.saturating_duration_since(now) > RELEASE_SLACK => {
                        earliest
                    }
                    _ => {
                        *released = Some(now);
                        return;
                    }
                }
            };
            tokio::time::sleep_until(wait_until).await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tracked_hosts = self.lock().last.len();
        f.debug_struct("RateLimiter")
            .field("min_interval", &self.min_interval)
            .field("tracked_hosts", &tracked_hosts)
            .finish()
    }
}

/// Rolls a host's reservation back if `wait` is cancelled before its sleep
/// completes and nobody has queued behind it. Created while the lock is
/// held (borrowing `&RateLimiter` and the caller's `host: &str`, so the
/// steady path allocates nothing) and disarmed once the sleep returns.
struct Reservation<'a> {
    limiter: &'a RateLimiter,
    host: &'a str,
    target: Instant,
    previous: Option<Instant>,
    armed: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.limiter.lock();
        // Only roll back if the map still holds exactly the slot this call
        // reserved: if a later caller already queued behind it, the map
        // holds a different value and that reservation must not be touched.
        if state
            .last
            .get(self.host)
            .is_some_and(|(slot, _)| *slot == self.target)
        {
            match self.previous {
                Some(previous) => {
                    if let Some((slot, _)) = state.last.get_mut(self.host) {
                        *slot = previous;
                    }
                }
                None => {
                    // Unreachable today: previous is None only for a
                    // host's first reservation, whose target is now, so
                    // wait() never reaches its await and clears `armed`
                    // first. Still the right thing to do if an await ever
                    // appears before that line.
                    state.last.remove(self.host);
                }
            }
        }
    }
}

impl State {
    fn maybe_prune(&mut self, now: Instant, min_interval: Duration) {
        if self.last.len() <= PRUNE_ABOVE_HOSTS {
            return;
        }
        if self.next_prune.is_some_and(|next| now < next) {
            return;
        }
        // A slot whose interval has fully elapsed is dead weight: the next
        // caller for that host gets `now` with or without it. An entry is
        // kept while either its reserved slot or its actual release is
        // still within the interval.
        if let Some(cutoff) = now.checked_sub(min_interval) {
            self.last.retain(|_, (slot, released)| {
                *slot > cutoff || released.is_some_and(|r| r > cutoff)
            });
        }
        let spacing = min_interval
            .saturating_mul(PRUNE_SPACING_MULTIPLE)
            .min(MAX_PRUNE_SPACING);
        self.next_prune = now.checked_add(spacing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked_hosts(limiter: &RateLimiter) -> usize {
        limiter.lock().last.len()
    }

    #[tokio::test(start_paused = true)]
    async fn first_call_does_not_wait() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        let start = Instant::now();
        limiter.wait("example.com").await;
        assert_eq!(Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn second_call_waits_out_the_interval() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        limiter.wait("example.com").await;
        let start = Instant::now();
        limiter.wait("example.com").await;
        assert_eq!(Instant::now() - start, Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn different_hosts_do_not_share_a_slot() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        limiter.wait("a.example").await;
        let start = Instant::now();
        limiter.wait("b.example").await;
        assert_eq!(Instant::now(), start);
    }

    #[tokio::test]
    async fn disabled_rate_limit_never_waits() {
        let limiter = RateLimiter::new(None);
        limiter.wait("example.com").await;
        // Would hang forever under start_paused if this accidentally slept.
        limiter.wait("example.com").await;
        assert_eq!(tracked_hosts(&limiter), 0);
    }

    #[tokio::test]
    async fn zero_interval_means_disabled() {
        let limiter = RateLimiter::new(Some(Duration::ZERO));
        assert!(limiter.min_interval.is_none());
        limiter.wait("example.com").await;
        assert_eq!(tracked_hosts(&limiter), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn huge_interval_is_clamped_not_disabled() {
        // `Duration::MAX` used to overflow the slot arithmetic and fall
        // back to "no wait"; the opposite of what was asked for.
        let limiter = RateLimiter::new(Some(Duration::MAX));
        limiter.wait("example.com").await;
        let start = Instant::now();
        limiter.wait("example.com").await;
        assert_eq!(Instant::now() - start, crate::MAX_DURATION);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_hosts_are_pruned_once_the_map_grows_large() {
        let interval = Duration::from_millis(10);
        let limiter = RateLimiter::new(Some(interval));

        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("host-{i}.example")).await;
        }
        assert_eq!(tracked_hosts(&limiter), PRUNE_ABOVE_HOSTS + 1);

        // Move well past every recorded slot's prune-eligible age, then
        // trigger the prune check with one more call.
        tokio::time::advance(interval * 2).await;
        limiter.wait("one-more.example").await;

        // Everything except the just-inserted host should have been swept.
        assert_eq!(tracked_hosts(&limiter), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn elapsed_slots_are_dropped() {
        // A polite crawler: 60 s per host, many hosts. A slot older than
        // one interval is dead weight and must not be kept for ten.
        let interval = Duration::from_secs(60);
        let limiter = RateLimiter::new(Some(interval));
        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("host-{i}.example")).await;
        }

        tokio::time::advance(interval + Duration::from_secs(1)).await;
        limiter.wait("one-more.example").await;
        assert_eq!(tracked_hosts(&limiter), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn prune_spacing_is_capped_for_long_intervals() {
        let interval = Duration::from_secs(60);
        let limiter = RateLimiter::new(Some(interval));
        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("first-{i}.example")).await;
        }
        tokio::time::advance(interval + Duration::from_secs(1)).await;
        limiter.wait("a.example").await;
        assert_eq!(tracked_hosts(&limiter), 1);

        // A second wave, one interval later: the previous prune must not
        // block this one for ten intervals.
        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("second-{i}.example")).await;
        }
        tokio::time::advance(interval + Duration::from_secs(1)).await;
        limiter.wait("b.example").await;
        assert_eq!(tracked_hosts(&limiter), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn pruning_never_drops_a_slot_that_is_still_live() {
        let interval = Duration::from_secs(60);
        let limiter = RateLimiter::new(Some(interval));
        limiter.wait("live.example").await;
        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("host-{i}.example")).await;
        }

        // Half an interval in, a prune runs; the live host must survive it
        // and still wait out the rest of its interval.
        tokio::time::advance(interval / 2).await;
        limiter.wait("trigger.example").await;
        let start = Instant::now();
        limiter.wait("live.example").await;
        assert_eq!(Instant::now() - start, interval / 2);
    }

    #[tokio::test(start_paused = true)]
    async fn prunes_are_spaced_out_when_hosts_stay_live() {
        let interval = Duration::from_millis(10);
        let spacing = interval * PRUNE_SPACING_MULTIPLE;
        let limiter = RateLimiter::new(Some(interval));

        for i in 0..=PRUNE_ABOVE_HOSTS {
            limiter.wait(&format!("host-{i}.example")).await;
        }

        // First prune runs, but every slot is still within its interval.
        tokio::time::advance(interval / 2).await;
        limiter.wait("x.example").await;
        assert_eq!(tracked_hosts(&limiter), PRUNE_ABOVE_HOSTS + 2);

        // Everything old is stale now, but the previous prune was less
        // than `spacing` ago: no scan yet.
        tokio::time::advance(interval).await;
        limiter.wait("y.example").await;
        assert_eq!(tracked_hosts(&limiter), PRUNE_ABOVE_HOSTS + 3);

        // Once `spacing` has passed since the last prune, the scan runs.
        tokio::time::advance(spacing).await;
        limiter.wait("z.example").await;
        assert_eq!(tracked_hosts(&limiter), 1); // only z
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_wait_gives_its_slot_back() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        let host = "example.com";
        limiter.wait(host).await;

        let timed_out = tokio::time::timeout(Duration::from_millis(10), limiter.wait(host)).await;
        assert!(timed_out.is_err());

        let start = Instant::now();
        limiter.wait(host).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(490));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_behind_a_later_reservation_keeps_it() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        let host = "example.com";
        let t0 = Instant::now();
        limiter.wait(host).await;

        let (first, _second) = tokio::join!(
            tokio::time::timeout(Duration::from_millis(10), limiter.wait(host)),
            limiter.wait(host)
        );
        assert!(first.is_err());
        assert_eq!(Instant::now() - t0, Duration::from_millis(1000));

        // The surviving (second) reservation must still be honoured in full,
        // not rolled back by the cancelled call that queued ahead of it.
        let start = Instant::now();
        limiter.wait(host).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn debug_output_names_no_hosts() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        limiter.wait("secret-host.example").await;
        let debug = format!("{limiter:?}");
        assert!(debug.contains("tracked_hosts: 1"), "{debug}");
        assert!(!debug.contains("secret-host"), "{debug}");
    }

    #[tokio::test(start_paused = true)]
    async fn completed_wait_is_not_rolled_back() {
        let limiter = RateLimiter::new(Some(Duration::from_millis(500)));
        let host = "example.com";
        limiter.wait(host).await;
        limiter.wait(host).await;

        let start = Instant::now();
        limiter.wait(host).await;
        assert_eq!(Instant::now() - start, Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_released_late_are_still_spaced() {
        let interval = Duration::from_millis(100);
        let limiter = std::sync::Arc::new(RateLimiter::new(Some(interval)));
        limiter.wait("example.com").await;
        let spawn = |l: std::sync::Arc<RateLimiter>| {
            tokio::spawn(async move {
                l.wait("example.com").await;
                Instant::now()
            })
        };
        let a = spawn(limiter.clone());
        let b = spawn(limiter.clone());
        tokio::task::yield_now().await;
        // A stalled runtime: both targets pass before either waiter runs.
        tokio::time::advance(Duration::from_secs(1)).await;
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a.max(b) - a.min(b) >= interval);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slightly_late_release_does_not_push_the_queue_back() {
        let interval = Duration::from_millis(10);
        let limiter = std::sync::Arc::new(RateLimiter::new(Some(interval)));
        let start = Instant::now();
        limiter.wait("example.com").await;
        let spawn = move |l: std::sync::Arc<RateLimiter>| {
            tokio::spawn(async move {
                l.wait("example.com").await;
                Instant::now() - start
            })
        };
        let a = spawn(limiter.clone());
        let b = spawn(limiter.clone());
        tokio::task::yield_now().await;
        // A's own release lands a full tick past its slot, as it would on
        // a busy runtime.
        tokio::time::advance(interval + Duration::from_millis(1)).await;
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(a, interval + Duration::from_millis(1));
        // B's slot was due one interval after A's actual release; a
        // release within one timer tick of that still counts as on time
        // and must not be pushed back again.
        assert!(b - interval * 2 < Duration::from_millis(1), "{b:?}");
    }
}
