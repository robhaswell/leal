//! The "user is interacting" signal, and waiting for it to clear (DESIGN
//! §3.10 rule 3).
//!
//! The app reports input; background jobs ask, at each checkpoint, whether
//! to wait. A waiting job sleeps on a condition variable rather than
//! polling, and wakes when input has been idle long enough, when the input
//! state changes, or when it is cancelled.
//!
//! Tests can stop the clock ([`Input::use_manual_clock`]): time then moves
//! only when the test moves it, so the pause tests check what the
//! scheduler does at each moment, not how fast a busy machine runs them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub(super) struct Input {
    state: Mutex<State>,
    /// Signalled when the input state changes or a job is cancelled.
    changed: Condvar,
    idle_after: Duration,
}

struct State {
    /// In the middle of a gesture ([`Input::set_interacting`]).
    interacting: bool,
    /// When input was last reported.
    last_input: Option<Instant>,
    /// Tests only: the time, if a test has stopped the clock.
    #[cfg(test)]
    manual_now: Option<Instant>,
}

impl State {
    /// The time now: the system's, or the test's stopped clock.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests have a stopped clock")
    )]
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.manual_now {
            return now;
        }
        Instant::now()
    }
}

impl Input {
    pub(super) fn new(idle_after: Duration) -> Self {
        Input {
            state: Mutex::new(State {
                interacting: false,
                last_input: None,
                #[cfg(test)]
                manual_now: None,
            }),
            changed: Condvar::new(),
            idle_after,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is two plain values, always consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One input event.
    pub(super) fn note(&self) {
        {
            let mut state = self.lock();
            state.last_input = Some(state.now());
        }
        self.changed.notify_all();
    }

    /// A gesture began or ended.
    pub(super) fn set_interacting(&self, interacting: bool) {
        {
            let mut state = self.lock();
            state.interacting = interacting;
            state.last_input = Some(state.now());
        }
        self.changed.notify_all();
    }

    /// Whether background work should wait now.
    pub(super) fn should_wait(&self) -> bool {
        self.remaining(&self.lock()).is_some()
    }

    /// Wakes every waiting job, so each checks its cancel flag. Taking the
    /// lock first means a job that has just checked its flag, and is about
    /// to wait, can't miss the wake-up.
    pub(super) fn wake(&self) {
        drop(self.lock());
        self.changed.notify_all();
    }

    /// Waits until background work may run, or `cancel` is set. Returns
    /// straight away if it may run now.
    pub(super) fn wait_until_idle(&self, cancel: &AtomicBool) {
        let mut state = self.lock();
        loop {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let Some(wait) = self.remaining(&state) else {
                return;
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// How much longer background work must wait, or `None` if it may run.
    /// During a gesture that is unknown, so it is the whole idle time (the
    /// waiter is woken when the gesture ends).
    fn remaining(&self, state: &State) -> Option<Duration> {
        if state.interacting {
            return Some(self.idle_after);
        }
        let since = state.now().saturating_duration_since(state.last_input?);
        self.idle_after
            .checked_sub(since)
            .filter(|left| !left.is_zero())
    }
}

/// Tests only: a clock that stands still until the test moves it.
#[cfg(test)]
impl Input {
    /// Stops the clock at the current time. From now on time moves only by
    /// [`advance_clock`](Self::advance_clock).
    pub(super) fn use_manual_clock(&self) {
        let mut state = self.lock();
        state.manual_now = Some(Instant::now());
    }

    /// Moves the stopped clock on by `by`, and wakes every waiting job so
    /// it sees the new time (a real waiter wakes when its timeout ends;
    /// with the clock stopped, that timeout means nothing).
    pub(super) fn advance_clock(&self, by: Duration) {
        {
            let mut state = self.lock();
            let now = state.manual_now.expect("the clock isn't stopped");
            state.manual_now = Some(now + by);
        }
        self.changed.notify_all();
    }
}
