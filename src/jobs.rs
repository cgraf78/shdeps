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

pub(crate) fn parallel_map<T, R, F>(items: &[T], jobs: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    if jobs <= 1 || items.len() <= 1 {
        return items.iter().map(f).collect();
    }

    let mut results = Vec::with_capacity(items.len());
    for chunk in items.chunks(jobs) {
        std::thread::scope(|scope| {
            let handles = chunk
                .iter()
                .map(|item| {
                    let f = &f;
                    scope.spawn(move || f(item))
                })
                .collect::<Vec<_>>();

            for handle in handles {
                // A panic inside a worker should behave like a panic in the
                // old serial code path, not get silently converted into a
                // partial result. Joining in spawn order also preserves caller
                // order even though the probes themselves overlap.
                match handle.join() {
                    Ok(result) => results.push(result),
                    Err(payload) => std::panic::resume_unwind(payload),
                }
            }
        });
    }
    results
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
    fn parallel_map_preserves_input_order() {
        let items = [3, 2, 1];

        assert_eq!(
            super::parallel_map(&items, 3, |item| item * 10),
            vec![30, 20, 10]
        );
    }
}
