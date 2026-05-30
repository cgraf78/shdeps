//! Shared bounded-concurrency helpers.
//!
//! shdeps has several hot paths where read-only probes are safe to overlap,
//! but install-time mutations are not. Keeping job-count parsing and chunked
//! fan-out here gives those callers one contract for `SHDEPS_JOBS`: explicit
//! positive values win, `1` is the deterministic sequential escape hatch, and
//! auto mode follows the host's CPU parallelism.

use std::collections::BTreeMap;

/// Returns the maximum parallel jobs for one shdeps operation.
#[must_use]
pub fn max(env_vars: &BTreeMap<String, String>) -> usize {
    let configured = env_vars
        .get("SHDEPS_JOBS")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if configured > 0 {
        // An explicit value is an operator decision, not a hint. This keeps
        // `SHDEPS_JOBS=1` useful for debugging and lets large machines or CI
        // exercise higher fan-out without fighting an internal cap.
        return configured;
    }

    // Auto mode follows the machine instead of an arbitrary ceiling. The work
    // this helper gates is intentionally read-only; callers that could trigger
    // rate limits or mutate shared state must choose narrower candidate sets
    // rather than hiding another policy here.
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

/// Returns the maximum parallel jobs for GitHub API reads.
#[must_use]
pub fn github_max(env_vars: &BTreeMap<String, String>) -> usize {
    const AUTO_CAP: usize = 4;

    let configured = env_vars
        .get("SHDEPS_JOBS")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if configured > 0 {
        return configured;
    }

    max(env_vars).min(AUTO_CAP)
}

pub(crate) fn parallel_map<T, R, F>(items: &[T], jobs: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if jobs <= 1 || items.len() <= 1 {
        return items.iter().map(f).collect();
    }

    // Worker pool, not chunked fan-out. The previous implementation
    // dispatched `items.chunks(jobs)` so a trailing chunk smaller than
    // `jobs` left workers idle while the last few items finished
    // serially — e.g., 5 items at `jobs=4` would finish 4 items in
    // round 1 and then run a single item in round 2 with 3 threads
    // idle. The pool below keeps every worker busy until the index
    // counter is exhausted, which is what users running `update`
    // against tens-to-hundreds of deps expect from `SHDEPS_JOBS`.
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let len = items.len();
    let next_index = AtomicUsize::new(0);
    // One mutex per result slot. Each slot is written exactly once by
    // one worker (uncontended), and read exactly once at the end on
    // the main thread (also uncontended), so the mutex is essentially
    // free here — it just avoids the unsafe of writing through raw
    // pointers into a shared `Vec`.
    let slots: Vec<Mutex<Option<R>>> = (0..len).map(|_| Mutex::new(None)).collect();
    let panic_payload: Mutex<Option<Box<dyn std::any::Any + Send>>> = Mutex::new(None);
    let worker_count = jobs.min(len);

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let f = &f;
            let next_index = &next_index;
            let slots = &slots;
            let panic_payload = &panic_payload;
            scope.spawn(move || {
                loop {
                    // Stop claiming new work as soon as ANY worker has
                    // recorded a panic. The serial path (`items.iter().map(f).collect()`)
                    // stops on first panic because `collect` drives the
                    // map lazily; the worker pool must match that
                    // "stop on first panic" contract so callers do not
                    // observe a wider blast radius when SHDEPS_JOBS goes
                    // from 1 to 2. In-flight items still complete (we
                    // cannot interrupt `f` once it has started), but no
                    // new index is claimed once a panic is recorded.
                    if panic_payload.lock().unwrap().is_some() {
                        return;
                    }
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= len {
                        return;
                    }
                    // Capture per-item panics rather than aborting the
                    // entire scope. The first captured payload is
                    // re-raised on the main thread after the scope
                    // ends, preserving the old "panic looks like the
                    // serial code path" contract.
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&items[index])));
                    match outcome {
                        Ok(result) => {
                            *slots[index].lock().unwrap() = Some(result);
                        }
                        Err(payload) => {
                            let mut held = panic_payload.lock().unwrap();
                            if held.is_none() {
                                *held = Some(payload);
                            }
                        }
                    }
                }
            });
        }
    });

    if let Some(payload) = panic_payload.into_inner().unwrap() {
        std::panic::resume_unwind(payload);
    }

    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap()
                .expect("every slot is written exactly once by a worker before scope ends")
        })
        .collect()
}

pub(crate) fn parallel_map_with_progress<T, R, F, P>(
    items: &[T],
    jobs: usize,
    f: F,
    mut progress: P,
) -> crate::Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
    P: FnMut(usize) -> crate::Result<()>,
{
    if jobs <= 1 || items.len() <= 1 {
        let mut results = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            results.push(f(item));
            progress(index + 1)?;
        }
        return Ok(results);
    }

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let len = items.len();
    let next_index = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = (0..len).map(|_| Mutex::new(None)).collect();
    let panic_payload: Mutex<Option<Box<dyn std::any::Any + Send>>> = Mutex::new(None);
    let worker_count = jobs.min(len);
    let (completed_tx, completed_rx) = mpsc::channel::<usize>();
    let mut progress_error = None;

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let f = &f;
            let next_index = &next_index;
            let slots = &slots;
            let panic_payload = &panic_payload;
            let completed_tx = completed_tx.clone();
            scope.spawn(move || {
                loop {
                    if panic_payload.lock().unwrap().is_some() {
                        return;
                    }
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= len {
                        return;
                    }
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&items[index])));
                    match outcome {
                        Ok(result) => {
                            *slots[index].lock().unwrap() = Some(result);
                            let _ = completed_tx.send(index);
                        }
                        Err(payload) => {
                            let mut held = panic_payload.lock().unwrap();
                            if held.is_none() {
                                *held = Some(payload);
                            }
                        }
                    }
                }
            });
        }
        drop(completed_tx);

        let mut completed = 0usize;
        while completed < len {
            match completed_rx.recv() {
                Ok(_) => {
                    completed += 1;
                    if let Err(error) = progress(completed) {
                        progress_error = Some(error);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    if let Some(payload) = panic_payload.into_inner().unwrap() {
        std::panic::resume_unwind(payload);
    }
    if let Some(error) = progress_error {
        return Err(error);
    }

    Ok(slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap()
                .expect("every slot is written exactly once by a worker before scope ends")
        })
        .collect())
}

/// Progress event for `parallel_map_with_item_progress`.
pub(crate) enum ItemProgressEvent<'a, R> {
    Started(usize),
    Completed { index: usize, result: &'a R },
}

/// Runs work in parallel, reports each started/completed item, and returns input order.
pub(crate) fn parallel_map_with_item_progress<T, R, F, P>(
    items: &[T],
    jobs: usize,
    f: F,
    mut progress: P,
) -> crate::Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
    P: FnMut(ItemProgressEvent<'_, R>) -> crate::Result<()>,
{
    if jobs <= 1 || items.len() <= 1 {
        let mut results = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            progress(ItemProgressEvent::Started(index))?;
            let result = f(item);
            progress(ItemProgressEvent::Completed {
                index,
                result: &result,
            })?;
            results.push(result);
        }
        return Ok(results);
    }

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let len = items.len();
    let next_index = AtomicUsize::new(0);
    let mut slots: Vec<Option<R>> = (0..len).map(|_| None).collect();
    let panic_payload: Mutex<Option<Box<dyn std::any::Any + Send>>> = Mutex::new(None);
    let worker_count = jobs.min(len);
    enum WorkerEvent<R> {
        Started(usize),
        Completed(usize, R),
    }

    let (completed_tx, completed_rx) = mpsc::channel::<WorkerEvent<R>>();
    let mut callback_error = None;

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let f = &f;
            let next_index = &next_index;
            let panic_payload = &panic_payload;
            let completed_tx = completed_tx.clone();
            scope.spawn(move || {
                loop {
                    if panic_payload.lock().unwrap().is_some() {
                        return;
                    }
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= len {
                        return;
                    }
                    let _ = completed_tx.send(WorkerEvent::Started(index));
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&items[index])));
                    match outcome {
                        Ok(result) => {
                            let _ = completed_tx.send(WorkerEvent::Completed(index, result));
                        }
                        Err(payload) => {
                            let mut held = panic_payload.lock().unwrap();
                            if held.is_none() {
                                *held = Some(payload);
                            }
                        }
                    }
                }
            });
        }
        drop(completed_tx);

        let mut completed = 0usize;
        while completed < len {
            match completed_rx.recv() {
                Ok(WorkerEvent::Started(index)) => {
                    if callback_error.is_none()
                        && let Err(error) = progress(ItemProgressEvent::Started(index))
                    {
                        callback_error = Some(error);
                    }
                }
                Ok(WorkerEvent::Completed(index, result)) => {
                    if callback_error.is_none()
                        && let Err(error) = progress(ItemProgressEvent::Completed {
                            index,
                            result: &result,
                        })
                    {
                        callback_error = Some(error);
                    }
                    slots[index] = Some(result);
                    completed += 1;
                }
                Err(_) => break,
            }
        }
    });

    if let Some(payload) = panic_payload.into_inner().unwrap() {
        std::panic::resume_unwind(payload);
    }
    if let Some(error) = callback_error {
        return Err(error);
    }

    Ok(slots
        .into_iter()
        .map(|slot| slot.expect("every slot is written exactly once by a worker before scope ends"))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[test]
    fn max_uses_explicit_values_without_auto_cap() {
        let mut env = BTreeMap::new();
        env.insert("SHDEPS_JOBS".to_owned(), "32".to_owned());

        assert_eq!(super::max(&env), 32);
    }

    #[test]
    fn max_keeps_one_as_sequential_escape_hatch() {
        let mut env = BTreeMap::new();
        env.insert("SHDEPS_JOBS".to_owned(), "1".to_owned());

        assert_eq!(super::max(&env), 1);
    }

    #[test]
    fn max_ignores_invalid_or_zero_values_for_auto_mode() {
        for value in ["0", "not-a-number"] {
            let mut env = BTreeMap::new();
            env.insert("SHDEPS_JOBS".to_owned(), value.to_owned());

            assert!(super::max(&env) >= 1);
        }
    }

    #[test]
    fn github_max_caps_auto_mode() {
        let env = BTreeMap::new();

        assert!(super::github_max(&env) <= 4);
        assert!(super::github_max(&env) >= 1);
    }

    #[test]
    fn github_max_respects_explicit_operator_choice() {
        let mut env = BTreeMap::new();
        env.insert("SHDEPS_JOBS".to_owned(), "8".to_owned());

        assert_eq!(super::github_max(&env), 8);

        env.insert("SHDEPS_JOBS".to_owned(), "1".to_owned());

        assert_eq!(super::github_max(&env), 1);
    }

    #[test]
    fn parallel_map_preserves_input_order() {
        let items = [3, 2, 1];

        assert_eq!(
            super::parallel_map(&items, 3, |item| item * 10),
            vec![30, 20, 10]
        );
    }

    #[test]
    fn parallel_map_keeps_workers_busy_past_chunk_boundary() {
        // Previously, 5 items at `jobs=4` would run 4 in round 1 and
        // then 1 in round 2 with 3 idle threads. Hold three first-wave
        // items until the trailing item starts; a chunked
        // implementation would deadlock and time out here because it
        // cannot start item 4 before items 1-3 finish.
        use std::sync::{Condvar, Mutex};
        use std::time::Duration;

        let items = [0, 1, 2, 3, 4];
        let first_wave_blocked = (Mutex::new(0usize), Condvar::new());
        let trailing_started = (Mutex::new(false), Condvar::new());
        let result = super::parallel_map(&items, 4, |item| {
            match *item {
                0 => {
                    let (lock, cvar) = &first_wave_blocked;
                    let blocked = lock.lock().unwrap();
                    let (blocked, _) = cvar
                        .wait_timeout_while(blocked, Duration::from_secs(5), |blocked| *blocked < 3)
                        .unwrap();
                    assert_eq!(
                        *blocked, 3,
                        "first-wave workers did not reach the blocking point"
                    );
                }
                1..=3 => {
                    let (lock, cvar) = &first_wave_blocked;
                    *lock.lock().unwrap() += 1;
                    cvar.notify_all();

                    let (lock, cvar) = &trailing_started;
                    let started = lock.lock().unwrap();
                    let (started, _) = cvar
                        .wait_timeout_while(started, Duration::from_secs(5), |started| !*started)
                        .unwrap();
                    assert!(
                        *started,
                        "trailing item was not claimed while first-wave workers were blocked"
                    );
                }
                4 => {
                    let (lock, cvar) = &trailing_started;
                    *lock.lock().unwrap() = true;
                    cvar.notify_all();
                }
                _ => unreachable!(),
            }
            *item
        });

        assert_eq!(result, items);
    }

    #[test]
    #[should_panic(expected = "boom")]
    fn parallel_map_resumes_panic_payload_on_main_thread() {
        // The serial code path lets a panic propagate to the caller;
        // the parallel path must preserve that contract so a panicking
        // probe does not silently produce a partial result.
        let items = [0, 1, 2, 3];
        let _ = super::parallel_map(&items, 4, |item| {
            if *item == 2 {
                panic!("boom");
            }
            *item
        });
    }

    #[test]
    fn parallel_map_stops_claiming_new_items_after_a_panic() {
        // Round-6 finding: the serial `items.iter().map(f).collect()`
        // path stops on first panic because `collect` drives the map
        // lazily. The worker pool must match that "stop on first
        // panic" contract so callers do not observe a wider blast
        // radius when SHDEPS_JOBS goes from 1 to 2+. This test
        // tracks how many items each closure call processes; after
        // the panic, no NEW indices should be claimed (in-flight
        // items still complete because we cannot interrupt `f`
        // mid-run).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let processed = AtomicUsize::new(0);
        // Many items so a "keep claiming after panic" bug would be
        // obvious in the processed count.
        let items: Vec<usize> = (0..1000).collect();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::parallel_map(&items, 4, |item| {
                // First worker to reach this point panics; the
                // others are sleeping so they reliably see the
                // recorded panic before claiming new indices.
                if *item == 0 {
                    // Brief sleep to give peers time to enter their
                    // first claim loop before the panic is recorded.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    panic!("boom-on-zero");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                processed.fetch_add(1, Ordering::Relaxed);
                *item
            })
        }));
        assert!(result.is_err(), "panic must propagate");
        // Without the fix, processed would approach 1000. With the
        // fix, at most one round of in-flight items per worker (~4)
        // completes after the panic. Generous bound to absorb
        // scheduler noise without flaking.
        let count = processed.load(Ordering::Relaxed);
        assert!(
            count < 50,
            "expected at most a handful of in-flight items past the panic, got {count}"
        );
    }
}
