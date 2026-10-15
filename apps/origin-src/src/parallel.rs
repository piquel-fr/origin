//! Running filesystem-heavy work on every core.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// Below this many items, threads cost more than they save.
pub const PARALLEL_THRESHOLD: usize = 64;

/// Maps `items` in order, on as many threads as there are cores when
/// there are enough items.
///
/// # Errors
///
/// Fails with an error `map` returned. A thread stops at its first error;
/// the others finish their remaining items first.
///
/// # Panics
///
/// Panics if `map` panics.
pub fn par_map<T: Sync, R: Send, E: Send>(
    items: &[T],
    map: impl Fn(&T) -> Result<R, E> + Sync,
) -> Result<Vec<R>, E> {
    if items.len() < PARALLEL_THRESHOLD {
        return items.iter().map(map).collect();
    }
    let workers = thread::available_parallelism().map_or(4, usize::from);
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(items.len()));
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| -> Result<(), E> {
                    let mut local = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        local.push((index, map(item)?));
                    }
                    results
                        .lock()
                        .expect("no worker panics holding the lock")
                        .extend(local);
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .try_for_each(|handle| handle.join().expect("worker threads do not panic"))
    })?;
    let mut results = results
        .into_inner()
        .expect("no worker panics holding the lock");
    results.sort_unstable_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}
