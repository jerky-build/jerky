//! Running a work list across a bounded set of threads.
//!
//! Three shapes, and what separates them is where the work comes from.
//!
//! [`drain`] takes a slice: tarball downloads, and the linker materialising a
//! plan's entries into the virtual store — a list that is the graph, where the
//! only interesting question is which failure gets reported. Those two wait on
//! different things, one on a network and one on a kernel, and are the same
//! shape of work, which is the shape this takes. [`crawl`]
//! takes a list its own workers extend as they go: the resolver's metadata
//! walk, where finishing one packument is what reveals the next ones. They
//! share a cap and nothing else — a growing list cannot be walked by an index,
//! cannot know when it is finished without asking what every worker is doing,
//! and has no lowest-indexed item for a failure to be attributed to.
//!
//! [`alongside`] takes a list a *different* thread fills while the workers
//! drain it: tarballs being downloaded while resolution is still deciding
//! which of them the tree wants. Its work is speculative — the producer has
//! not finished answering the question the work is for — so unlike the other
//! two it does not run to completion. It stops when the producer does, and
//! what is left on the list is abandoned rather than drained.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// Run `body` over every item, at most `workers` at a time, stopping early
/// once one fails.
///
/// The error returned is the one from the **lowest-indexed** item that failed,
/// which is what makes a failure reproducible rather than a report on which
/// thread happened to lose. Two properties give that:
///
/// - Every claim is a `fetch_add`, so the claimed indices are a contiguous
///   prefix of `items` however the workers interleave. The abort flag decides
///   only how far that prefix extends, never which indices are in it.
/// - The lowest index that fails is therefore always inside the prefix — the
///   prefix stopped growing *because* something in it failed — so the lowest
///   failing index is the lowest failing index overall, not merely the lowest
///   among those that happened to be attempted.
///
/// Work already in flight when the flag is set still finishes. Callers that
/// care must therefore tolerate a body completing after another has failed;
/// what is guaranteed is that no *new* item is claimed.
pub fn drain<T, E, F>(items: &[T], workers: usize, body: F) -> Result<(), E>
where
    T: Sync,
    E: Send,
    F: Fn(&T) -> Result<(), E> + Sync,
{
    // No more threads than there is work: a two-item list should not start
    // sixteen of them.
    let workers = workers.min(items.len());
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let failures: Mutex<BTreeMap<usize, E>> = Mutex::new(BTreeMap::new());

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while !failed.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };

                    if let Err(err) = body(item) {
                        failed.store(true, Ordering::Relaxed);
                        failures.lock().unwrap().insert(index, err);
                    }
                }
            });
        }
    });

    match failures.into_inner().unwrap().into_values().next() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A work list the workers draining it may add to while they drain it.
///
/// Handed to [`crawl`]'s body, and the only way to extend a run in progress.
pub struct Worklist<T> {
    /// How many workers are drawing from this list. Fixed for the run, and
    /// what "everyone is idle" is measured against.
    workers: usize,
    state: Mutex<WorklistState<T>>,
    wake: Condvar,
}

struct WorklistState<T> {
    queue: Vec<T>,
    /// Workers parked with nothing to take.
    ///
    /// A worker that is *not* parked here is either inside the body or on its
    /// way into it, and either way may still push. So the run is over exactly
    /// when the queue is empty and this reaches `workers` — a condition that
    /// has to be evaluated under the lock, because both halves of it move.
    idle: usize,
    /// Set once the run is over, and never cleared. Without it a worker woken
    /// by the final `notify_all` would park again and never leave.
    done: bool,
}

impl<T> Worklist<T> {
    /// Add an item. It may be taken by any worker, including the one pushing.
    pub fn push(&self, item: T) {
        self.state.lock().unwrap().queue.push(item);
        self.wake.notify_one();
    }

    /// End the run now, leaving whatever is still on the list untaken.
    ///
    /// [`drain`]'s early stop, in the form a growing list can have one. A
    /// crawl has no failure of its own to report, so it cannot decide to stop
    /// on an error the way `drain` does; only the body knows whether what it
    /// just found makes the rest of the run pointless.
    ///
    /// Work already claimed still finishes, exactly as in `drain`: what is
    /// guaranteed is that no *new* item is taken.
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.done = true;
        self.wake.notify_all();
    }

    /// The next item, or `None` once the run is over.
    fn take(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        loop {
            // Checked before the queue, not after, so `stop` takes effect
            // against a list that still has items on it — which is the only
            // situation it is ever called in.
            if state.done {
                return None;
            }
            // LIFO. Nothing depends on the order — a crawl's callers key their
            // own memo — and taking the most recently discovered item first is
            // what walks down a long dependency chain rather than fanning the
            // shallow levels out before starting on it. The chain is the
            // critical path, so reaching it early is the whole point.
            if let Some(item) = state.queue.pop() {
                return Some(item);
            }

            state.idle += 1;
            if state.idle == self.workers {
                // Every worker is parked here with an empty queue, under this
                // lock, so no worker is inside the body and nothing more can
                // arrive. This is the only place a run ends.
                state.done = true;
                self.wake.notify_all();
                return None;
            }

            state = self.wake.wait(state).unwrap();
            state.idle -= 1;
        }
    }
}

/// Run `body` over a work list that grows as it runs, at most `workers` at a
/// time, until nothing is left.
///
/// The cap is **not** lowered to the number of seeds, which is the one place
/// this differs from [`drain`] in a way that matters rather than in mechanism.
/// A fixed list of two items never needs more than two threads; a crawl seeded
/// with one item may fan out to thousands, and capping at the seed count would
/// pin it to a single thread for the whole run.
///
/// There is no error path, and that is deliberate rather than unfinished. A
/// crawl's caller runs it for its side effects on a memo, so a failing item is
/// simply one that leaves nothing behind — whoever needs that item later finds
/// the memo empty, asks for it again, and reports the failure from wherever it
/// can say something useful about it. Attributing a failure here would mean
/// choosing between the failures of a list that has no fixed order to choose
/// by.
///
/// What a body that has hit something fatal should do instead is call
/// [`Worklist::stop`]. Not doing so is how a crawl turns a typo'd dependency
/// into a walk of the entire graph before anything is reported, which is the
/// half of `drain`'s early stop that does carry over.
pub fn crawl<T, F>(seeds: Vec<T>, workers: usize, body: F)
where
    T: Send,
    F: Fn(T, &Worklist<T>) + Sync,
{
    if seeds.is_empty() {
        return;
    }

    let workers = workers.max(1);
    let list = Worklist {
        workers,
        state: Mutex::new(WorklistState {
            queue: seeds,
            idle: 0,
            done: false,
        }),
        wake: Condvar::new(),
    };

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(item) = list.take() {
                    body(item, &list);
                }
            });
        }
    });
}

/// A work list one thread fills while others drain it.
///
/// Handed to [`alongside`]'s producer, and the only way into a run in
/// progress. Unlike a [`Worklist`] it has no opinion about when the run is
/// over: a list nobody has pushed to yet is not a finished one, it is one
/// whose producer has not got there, so the run ends when the producer says so
/// and at no other time.
pub struct Feed<T> {
    state: Mutex<FeedState<T>>,
    wake: Condvar,
}

struct FeedState<T> {
    queue: Vec<T>,
    /// Set when the producer returns, and never cleared.
    done: bool,
}

impl<T> Feed<T> {
    /// Add an item, to be taken by whichever worker is free.
    pub fn push(&self, item: T) {
        self.state.lock().unwrap().queue.push(item);
        self.wake.notify_one();
    }

    /// End the run, abandoning whatever is still on the list.
    ///
    /// Abandoning rather than draining is the contract, and it is the whole
    /// reason this shape is worth having over a channel joined at the end.
    /// What runs alongside a producer is speculative by construction — the
    /// producer is still deciding what is actually wanted — so an item still
    /// queued when the producer finishes is one whose value has just been
    /// settled by something else. Finishing it is at best duplicated work and
    /// at worst work on something the producer's answer excludes.
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.done = true;
        self.wake.notify_all();
    }

    /// The next item, or `None` once the run is over.
    fn take(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        loop {
            // Before the queue, so that `stop` takes effect against a list
            // that still has items on it — which is the situation it is
            // always called in.
            if state.done {
                return None;
            }
            // LIFO, as [`crawl`] is and for a weaker version of the same
            // reason: a producer discovers work roughly in the order it wants
            // it, so the most recently pushed item is the one most likely to
            // still matter by the time the run ends.
            if let Some(item) = state.queue.pop() {
                return Some(item);
            }

            state = self.wake.wait(state).unwrap();
        }
    }
}

/// Run `body` over a list that `produce` fills while it runs, and return what
/// `produce` returned.
///
/// The third shape, and the one whose work is known neither before it starts
/// nor by the workers themselves: it belongs to the producer, which is doing
/// something else entirely and pushing what it happens to learn on the way.
/// That makes the run **speculative**, and everything about the contract
/// follows from it. There is no error path, because a caller running this for
/// a side effect has somewhere better to report a failure — the same argument
/// [`crawl`] makes. There is no join, because [`Feed::stop`] abandons what is
/// left rather than draining it.
///
/// `produce` runs on the calling thread, so the value it returns needs no
/// channel to get out, and a `?` inside it does what it looks like it does.
///
/// Work already claimed when the producer returns still finishes, exactly as
/// in [`drain`] and [`crawl`]. A body that may block for a long time is
/// therefore a body the producer will wait on at the end, which bounds how
/// long this can hold up its caller to one item per worker.
pub fn alongside<T, R, F, P>(workers: usize, body: F, produce: P) -> R
where
    T: Send,
    F: Fn(T) + Sync,
    P: FnOnce(&Feed<T>) -> R,
{
    let workers = workers.max(1);
    let feed = Feed {
        state: Mutex::new(FeedState {
            queue: Vec::new(),
            done: false,
        }),
        wake: Condvar::new(),
    };

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(item) = feed.take() {
                    body(item);
                }
            });
        }

        // A guard rather than a call after `produce`, because `thread::scope`
        // joins on the unwind path too: a producer that panics would otherwise
        // leave every worker parked on a list nobody will ever close, turning
        // a panic into a hang.
        let _stop = StopOnDrop(&feed);
        produce(&feed)
    })
}

/// Ends a [`Feed`]'s run however its producer left.
struct StopOnDrop<'a, T>(&'a Feed<T>);

impl<T> Drop for StopOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn the_lowest_indexed_failure_is_the_one_reported() {
        // Two failures far apart, and enough repetitions that a pool reporting
        // whichever finished first would be caught.
        for _ in 0..50 {
            let items: Vec<usize> = (0..40).collect();
            let err = drain(&items, 16, |item| {
                if *item == 3 || *item == 30 {
                    Err(*item)
                } else {
                    Ok(())
                }
            })
            .unwrap_err();

            assert_eq!(err, 3);
        }
    }

    #[test]
    fn a_failure_stops_new_work_being_claimed() {
        // Not a timing assertion: the first item fails, and with a cap of one
        // there is nothing in flight to race it, so every later item is
        // provably unclaimed rather than merely likely to be.
        let items: Vec<usize> = (0..40).collect();
        let claimed = Mutex::new(Vec::new());

        let err = drain(&items, 1, |item| {
            claimed.lock().unwrap().push(*item);
            if *item == 0 { Err(*item) } else { Ok(()) }
        })
        .unwrap_err();

        assert_eq!(err, 0);
        assert_eq!(
            claimed.into_inner().unwrap(),
            [0],
            "the pool kept claiming work after a failure"
        );
    }

    #[test]
    fn every_item_runs_when_none_fail() {
        let items: Vec<usize> = (0..100).collect();
        let seen = Mutex::new(Vec::new());

        drain(&items, 16, |item| {
            seen.lock().unwrap().push(*item);
            Ok::<(), ()>(())
        })
        .unwrap();

        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        assert_eq!(seen, items);
    }

    #[test]
    fn an_empty_list_starts_no_threads_and_succeeds() {
        let items: Vec<usize> = Vec::new();
        drain(&items, 16, |_| Ok::<(), ()>(())).unwrap();
    }

    #[test]
    fn a_crawl_runs_work_its_own_body_discovers() {
        // A binary tree eight levels deep, seeded with its root alone. Nothing
        // but the run itself knows the list, which is the whole difference
        // from `drain`.
        let seen = Mutex::new(Vec::new());

        crawl(vec![1usize], 8, |node, work| {
            seen.lock().unwrap().push(node);
            if node < 128 {
                work.push(node * 2);
                work.push(node * 2 + 1);
            }
        });

        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        assert_eq!(seen, (1..256).collect::<Vec<_>>());
    }

    #[test]
    fn a_crawl_seeded_with_one_item_still_reaches_its_full_width() {
        // The property `drain`'s `workers.min(items.len())` would destroy. One
        // seed fans out to eight, and each of the eight waits for the other
        // seven — so a pool that sized itself from the seed count would park
        // on the first of them and never get to the second.
        let arrived = Mutex::new(0usize);
        let all_here = Condvar::new();
        let timed_out = AtomicBool::new(false);

        crawl(vec![None], 8, |item: Option<()>, work| {
            if item.is_none() {
                for _ in 0..8 {
                    work.push(Some(()));
                }
                return;
            }

            let mut count = arrived.lock().unwrap();
            *count += 1;
            if *count >= 8 {
                all_here.notify_all();
                return;
            }
            let (_guard, result) = all_here
                .wait_timeout_while(count, std::time::Duration::from_secs(5), |count| *count < 8)
                .unwrap();
            if result.timed_out() {
                // Releases the others rather than making each pay the timeout.
                timed_out.store(true, Ordering::Relaxed);
                all_here.notify_all();
            }
        });

        assert!(
            !timed_out.load(Ordering::Relaxed),
            "eight discovered items never ran together, so the width came from \
             the seed count rather than from the cap"
        );
    }

    #[test]
    fn a_crawl_down_a_chain_ends_rather_than_parking_forever() {
        // Every worker but one is idle for the whole run, so this is the case
        // the idle count exists to get right: the run must end when the last
        // link finishes, not when some worker happens to be woken.
        let seen = Mutex::new(Vec::new());

        crawl(vec![0usize], 16, |step, work| {
            seen.lock().unwrap().push(step);
            if step < 200 {
                work.push(step + 1);
            }
        });

        assert_eq!(seen.into_inner().unwrap().len(), 201);
    }

    #[test]
    fn an_empty_crawl_starts_no_threads() {
        crawl(Vec::<usize>::new(), 16, |_, _| unreachable!("no seeds"));
    }

    #[test]
    fn stopping_a_crawl_leaves_the_rest_of_the_list_untaken() {
        // Not a timing assertion: one worker, and the first item it takes
        // stops the run, so every later item is provably unclaimed rather than
        // merely likely to be. The same shape as
        // `a_failure_stops_new_work_being_claimed`, which is the property this
        // restores to the growing list.
        let seen = Mutex::new(Vec::new());

        crawl(vec![0usize], 1, |step, work| {
            seen.lock().unwrap().push(step);
            for next in 1..=10 {
                work.push(step + next);
            }
            if step == 0 {
                work.stop();
            }
        });

        assert_eq!(
            seen.into_inner().unwrap(),
            [0],
            "the crawl kept taking work after it was stopped"
        );
    }

    #[test]
    fn a_stopped_crawl_ends_even_with_workers_parked() {
        // Fifteen of sixteen workers are parked on the condvar while the
        // sixteenth decides to stop. They have to be woken, or the run never
        // returns.
        let seen = Mutex::new(0usize);

        crawl(vec![0usize], 16, |step, work| {
            *seen.lock().unwrap() += 1;
            if step < 3 {
                work.push(step + 1);
            } else {
                work.push(99);
                work.stop();
            }
        });

        assert!(*seen.lock().unwrap() >= 4);
    }

    #[test]
    fn a_producer_feeds_workers_that_are_already_running() {
        let seen = Mutex::new(Vec::new());
        let done = AtomicUsize::new(0);

        let produced = alongside(
            4,
            |item: usize| {
                seen.lock().unwrap().push(item);
                done.fetch_add(1, Ordering::SeqCst);
            },
            |feed| {
                for item in 0..100 {
                    feed.push(item);
                }
                // The producer waits for the workers rather than returning
                // straight away, which is what makes this a test that they run
                // *alongside* it: nothing has closed the list, so if the pool
                // only started draining once the producer was done, nothing
                // would ever arrive. Bounded, so that a pool which never
                // starts fails the assertion below instead of hanging.
                let deadline = Instant::now() + Duration::from_secs(5);
                while done.load(Ordering::SeqCst) < 100 && Instant::now() < deadline {
                    std::thread::yield_now();
                }
                "produced"
            },
        );

        assert_eq!(produced, "produced");
        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        assert_eq!(seen, (0..100).collect::<Vec<usize>>());
    }

    #[test]
    fn what_is_still_queued_when_the_producer_returns_is_abandoned() {
        // A timing assertion, and deliberately one. The producer's return
        // *is* the stop, so whether a worker claims one more item at that
        // exact instant is a race nothing outside the pool can observe — but
        // the scale is not a race. Draining this list takes a hundred
        // seconds; abandoning it takes one item's worth of sleep, and four
        // orders of magnitude is not something a scheduler decides.
        let seen = AtomicUsize::new(0);
        let started = Instant::now();

        alongside(
            1,
            |_: usize| {
                std::thread::sleep(Duration::from_millis(10));
                seen.fetch_add(1, Ordering::SeqCst);
            },
            |feed| {
                for item in 0..10_000 {
                    feed.push(item);
                }
            },
        );

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the run drained its list instead of abandoning it, after {:?} and {} items",
            started.elapsed(),
            seen.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn a_producer_that_panics_ends_the_run_rather_than_hanging() {
        // `thread::scope` joins on the unwind path as well as the normal one,
        // so a producer that panics without closing the list would leave every
        // worker parked on the condvar and turn the panic into a hang. Run on
        // a thread of its own and waited for with a deadline, so that a
        // regression fails this test instead of stopping the suite.
        let (finished, panicked) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(|| {
                alongside(
                    4,
                    |_: usize| {},
                    |feed| {
                        feed.push(1);
                        panic!("the producer gave up");
                    },
                )
            });
            let _ = finished.send(outcome.is_err());
        });

        assert_eq!(
            panicked.recv_timeout(Duration::from_secs(10)),
            Ok(true),
            "a panicking producer left the workers parked"
        );
    }
}
