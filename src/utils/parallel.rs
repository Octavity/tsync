use std::{
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
};

use crate::errors::{Error, Result};

/// Normalizes a requested thread count so it can never be zero or absurdly large
/// relative to the amount of work to be done.
fn worker_count(num_threads: usize, jobs: usize) -> usize {
    num_threads.max(1).min(jobs.max(1))
}

/// Runs `work` over `jobs` across up to `num_threads` worker threads, chunking the
/// job list so no more than `num_threads` threads are ever spawned.
///
/// Unlike a naive "propagate first error immediately" loop, this always drains all
/// results and joins every thread before returning, so a failing job can't leave
/// sibling workers running unsupervised in the background (e.g. still writing files
/// to the target directory) after `run()` has already reported failure to the caller.
///
/// If multiple jobs fail, the first error encountered (by receive order) is returned;
/// a worker thread panicking is reported as an `Error::descriptive` if no other error
/// was already recorded.
pub fn run_parallel<T, F>(jobs: Vec<T>, num_threads: usize, work: F) -> Result<()>
where
    T: Send + 'static + Clone,
    F: Fn(T) -> Result<()> + Send + Sync + 'static,
{
    if jobs.is_empty() {
        return Ok(());
    }

    let num_threads = worker_count(num_threads, jobs.len());
    let chunk_size = jobs.len().div_ceil(num_threads).max(1);
    let work = Arc::new(work);

    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::<JoinHandle<()>>::new();

    for chunk in jobs.chunks(chunk_size) {
        let tx = tx.clone();
        let chunk = chunk.to_vec();
        let work = work.clone();

        let handle = thread::spawn(move || {
            for job in chunk {
                let result = work(job);
                let _ = tx.send(result);
            }
        });

        handles.push(handle);
    }

    drop(tx);

    // Drain all results first so we don't return early while sibling threads
    // are still mid-flight.
    let mut first_err: Option<Error> = None;
    for result in rx {
        if let Err(e) = result {
            first_err.get_or_insert(e);
        }
    }

    for handle in handles {
        if handle.join().is_err() {
            first_err.get_or_insert(Error::descriptive("A worker thread panicked"));
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Map-flavored counterpart of [`run_parallel`]: runs `work` over `jobs` across up to
/// `num_threads` worker threads and collects every result into a `Vec`.
///
/// Results are always returned in the same order as `jobs`, regardless of which worker
/// finished first, so callers can rely on positional correspondence with their input.
///
/// Failure semantics match [`run_parallel`]: all workers are drained and joined before
/// returning, and the first error (by receive order) is returned with the partial results
/// discarded. Use this only when a single failing job should abort the whole batch; if
/// per-job failures are expected (e.g. one unreadable file in a library), map those to a
/// value describing the failure and keep the batch fallible only for real errors.
pub fn map_parallel<T, U, F>(jobs: Vec<T>, num_threads: usize, work: F) -> Result<Vec<U>>
where
    T: Send + 'static + Clone,
    U: Send + 'static,
    F: Fn(T) -> Result<U> + Send + Sync + 'static,
{
    if jobs.is_empty() {
        return Ok(Vec::new());
    }

    let total = jobs.len();
    let num_threads = worker_count(num_threads, total);
    let chunk_size = total.div_ceil(num_threads).max(1);
    let work = Arc::new(work);

    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::<JoinHandle<()>>::new();

    for (chunk_index, chunk) in jobs.chunks(chunk_size).enumerate() {
        let tx = tx.clone();
        let chunk = chunk.to_vec();
        let work = work.clone();
        let offset = chunk_index * chunk_size;

        let handle = thread::spawn(move || {
            for (local_index, job) in chunk.into_iter().enumerate() {
                let result = work(job);
                let _ = tx.send((offset + local_index, result));
            }
        });

        handles.push(handle);
    }

    drop(tx);

    // Collect by original index so worker scheduling order can't reorder the output.
    let mut slots: Vec<Option<U>> = (0..total).map(|_| None).collect();
    let mut first_err: Option<Error> = None;

    for (index, result) in rx {
        match result {
            Ok(value) => slots[index] = Some(value),
            Err(e) => {
                first_err.get_or_insert_with(|| e.with_context(format!("While processing job {index}")));
            }
        }
    }

    for handle in handles {
        if handle.join().is_err() {
            first_err.get_or_insert(Error::descriptive("A worker thread panicked"));
        }
    }

    if let Some(e) = first_err {
        return Err(e);
    }

    slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| slot.ok_or_else(|| Error::descriptive(format!("Job {index} produced no result"))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{map_parallel, run_parallel};
    use crate::errors::{Error, Result};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn map_parallel_preserves_job_order() {
        let jobs: Vec<u32> = (0..1_000).collect();
        // Earlier jobs sleep longer, so workers naturally finish out of order.
        let results = map_parallel(jobs.clone(), 8, |job| {
            std::thread::sleep(std::time::Duration::from_micros((1_000 - job) as u64));
            Ok(job * 2)
        })
        .expect("map should succeed");

        assert_eq!(results, jobs.iter().map(|job| job * 2).collect::<Vec<_>>());
    }

    #[test]
    fn map_parallel_runs_work_on_every_job() {
        let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
        let seen_handle = seen.clone();
        let jobs: Vec<u32> = (0..64).collect();

        let results = map_parallel(jobs, 4, move |job| {
            seen_handle.lock().unwrap().push(job);
            Ok(job)
        })
        .expect("map should succeed");

        assert_eq!(results, (0..64).collect::<Vec<_>>());

        let mut seen = seen.lock().unwrap().clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn map_parallel_handles_empty_and_zero_thread_inputs() {
        let empty: Vec<u32> = map_parallel(Vec::new(), 4, |job: u32| Ok(job)).expect("empty map should succeed");

        assert!(empty.is_empty());

        let results = map_parallel(vec![1, 2, 3], 0, |job| Ok(job * 10)).expect("map should succeed");
        assert_eq!(results, vec![10, 20, 30]);
    }

    #[test]
    fn map_parallel_reports_error_after_draining_all_jobs() {
        let completed = Arc::new(AtomicUsize::new(0));
        let completed_handle = completed.clone();

        let result = map_parallel((0..100).collect::<Vec<u32>>(), 4, move |job| -> Result<u32> {
            if job == 0 {
                return Err(Error::descriptive("boom"));
            }

            completed_handle.fetch_add(1, Ordering::SeqCst);
            Ok(job)
        });

        let error = result.expect_err("one failing job should fail the batch");
        assert!(error.message.contains("boom"));

        // Unlike an early-returning loop, every remaining job still ran to completion.
        assert_eq!(completed.load(Ordering::SeqCst), 99);
    }

    #[test]
    fn run_parallel_reports_first_error_and_drains() {
        let completed = Arc::new(AtomicUsize::new(0));
        let completed_handle = completed.clone();

        let result = run_parallel((0..50).collect::<Vec<u32>>(), 4, move |job| -> Result<()> {
            if job == 7 {
                return Err(Error::descriptive("bad job"));
            }

            completed_handle.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(completed.load(Ordering::SeqCst), 49);
    }
}
