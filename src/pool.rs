//! Running a fixed work list across a bounded set of threads.
//!
//! Both of jerky's concurrent phases have the same shape — a slice of work
//! known up front, a cap on how much of it is in flight, and a requirement
//! that the failure reported is the same one on every run — so the shape lives
//! here once rather than being written out at each of them. What differs is
//! only the body and the error type.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

#[cfg(test)]
mod tests {
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
}
