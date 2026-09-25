//! Bounded parallel transformations with deterministic publication on the caller thread.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Condvar, Mutex, mpsc};
use std::thread;

/// Limits covering input/output buffers retained until ordered publication.
#[derive(Clone, Copy)]
pub struct Limits {
    /// Number of independent transformation workers.
    pub workers: usize,
    /// Maximum simultaneously admitted input/output bytes, excluding decoder scratch.
    pub bytes: usize,
    /// Maximum admitted jobs, including completed jobs waiting for earlier results.
    pub jobs: usize,
}

/// High-water marks for admitted jobs and buffers.
#[derive(Debug, Default)]
pub struct Stats {
    /// Number of successfully published results.
    pub published: usize,
    /// Maximum admitted byte charge.
    pub peak_bytes: usize,
    /// Maximum admitted job count.
    pub peak_jobs: usize,
}

/// Shared queue and admission accounting.
struct State<I> {
    /// Jobs awaiting workers, with sequence and memory charge.
    queue: VecDeque<(usize, usize, I)>,
    /// Bytes charged across queued, running and completed jobs.
    bytes: usize,
    /// Jobs charged across queued, running and completed jobs.
    jobs: usize,
    /// Peak admitted bytes.
    peak_bytes: usize,
    /// Peak admitted jobs.
    peak_jobs: usize,
    /// Producer reached end of input.
    finished: bool,
    /// A failure or caller unwind stops admission and workers.
    cancelled: bool,
}

/// Queue synchronization shared by the producer and workers.
struct Shared<I> {
    /// Queue state, never held while performing I/O or transforming data.
    state: Mutex<State<I>>,
    /// Wakeups for new work, released capacity and cancellation.
    changed: Condvar,
}

impl<I> Shared<I> {
    /// Wake every blocked participant after failure or caller unwind.
    fn cancel(&self) {
        self.state.lock().unwrap().cancelled = true;
        self.changed.notify_all();
    }
}

/// Cancel scoped participants if the publisher unwinds.
struct CancelOnDrop<'a, I>(&'a Shared<I>);

impl<I> Drop for CancelOnDrop<'_, I> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Run bounded jobs concurrently and publish results in original input order.
/// The producer supplies small descriptors; workers acquire source bytes within the charge.
pub fn run<I, O, P, F, W>(
    limits: Limits,
    produce: P,
    transform: F,
    mut publish: W,
) -> Result<Stats, String>
where
    I: Send,
    O: Send,
    P: FnOnce(&mut dyn FnMut(I, usize) -> Result<(), String>) -> Result<(), String> + Send,
    F: Fn(I) -> Result<O, String> + Sync,
    W: FnMut(O) -> Result<(), String>,
{
    if limits.workers == 0 || limits.bytes == 0 || limits.jobs == 0 {
        return Err("pipeline limits must be positive".into());
    }
    let shared = Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            bytes: 0,
            jobs: 0,
            peak_bytes: 0,
            peak_jobs: 0,
            finished: false,
            cancelled: false,
        }),
        changed: Condvar::new(),
    };
    thread::scope(|scope| {
        let _cancel = CancelOnDrop(&shared);
        let (sender, receiver) = mpsc::channel::<Result<(usize, usize, O), String>>();
        let producer_sender = sender.clone();
        let state = &shared;
        scope.spawn(move || {
            let mut sequence = 0;
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                produce(&mut |input, bytes| {
                    let bytes = bytes.max(1);
                    if bytes > limits.bytes {
                        return Err(format!(
                            "job requires {bytes} bytes, exceeding pipeline budget {}",
                            limits.bytes
                        ));
                    }
                    let mut s = state.state.lock().unwrap();
                    while !s.cancelled && (s.jobs == limits.jobs || bytes > limits.bytes - s.bytes)
                    {
                        s = state.changed.wait(s).unwrap();
                    }
                    if s.cancelled {
                        return Err("pipeline cancelled".into());
                    }
                    s.bytes += bytes;
                    s.jobs += 1;
                    s.peak_bytes = s.peak_bytes.max(s.bytes);
                    s.peak_jobs = s.peak_jobs.max(s.jobs);
                    s.queue.push_back((sequence, bytes, input));
                    sequence += 1;
                    state.changed.notify_all();
                    Ok(())
                })
            }))
            .unwrap_or_else(|_| Err("pipeline producer panicked".into()));
            if let Err(error) = result {
                let _ = producer_sender.send(Err(error));
                state.cancel();
            }
            state.state.lock().unwrap().finished = true;
            state.changed.notify_all();
        });
        for _ in 0..limits.workers {
            let sender = sender.clone();
            let transform = &transform;
            scope.spawn(move || {
                loop {
                    let job = {
                        let mut s = state.state.lock().unwrap();
                        loop {
                            if s.cancelled {
                                return;
                            }
                            if let Some(job) = s.queue.pop_front() {
                                break job;
                            }
                            if s.finished {
                                return;
                            }
                            s = state.changed.wait(s).unwrap();
                        }
                    };
                    let (sequence, bytes, input) = job;
                    let result = panic::catch_unwind(AssertUnwindSafe(|| transform(input)))
                        .unwrap_or_else(|_| Err("pipeline worker panicked".into()));
                    match result {
                        Ok(output) => {
                            if sender.send(Ok((sequence, bytes, output))).is_err() {
                                state.cancel();
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            state.cancel();
                            return;
                        }
                    }
                }
            });
        }
        drop(sender);
        let mut pending = BTreeMap::new();
        let mut next = 0;
        for message in receiver {
            let (sequence, bytes, output) = message?;
            pending.insert(sequence, (bytes, output));
            while let Some((bytes, output)) = pending.remove(&next) {
                publish(output)?;
                next += 1;
                let mut s = shared.state.lock().unwrap();
                s.bytes -= bytes;
                s.jobs -= 1;
                shared.changed.notify_all();
            }
        }
        let s = shared.state.lock().unwrap();
        if s.cancelled || !pending.is_empty() || s.jobs != 0 {
            return Err("pipeline ended before ordered publication completed".into());
        }
        Ok(Stats {
            published: next,
            peak_bytes: s.peak_bytes,
            peak_jobs: s.peak_jobs,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Out-of-order workers preserve publication order and both capacity limits.
    #[test]
    fn ordered_and_bounded() {
        let mut output = Vec::new();
        let stats = run(
            Limits {
                workers: 4,
                bytes: 64,
                jobs: 3,
            },
            |emit| {
                for i in 0..100 {
                    emit(i, 20)?;
                }
                Ok(())
            },
            |i| {
                if i == 0 {
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(i)
            },
            |i| {
                output.push(i);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output, (0..100).collect::<Vec<_>>());
        assert_eq!(stats.published, 100);
        assert!(stats.peak_bytes <= 64 && stats.peak_jobs <= 3);
    }

    /// Errors and panics cancel a blocked producer without deadlocking scoped joins.
    #[test]
    fn failures_release_backpressure() {
        for failure in 0..5 {
            let result = panic::catch_unwind(|| {
                run(
                    Limits {
                        workers: 3,
                        bytes: 2,
                        jobs: 2,
                    },
                    |emit| {
                        for i in 0..100 {
                            if failure == 3 && i == 3 {
                                return Err("producer failure".into());
                            }
                            if failure == 4 && i == 3 {
                                panic!("producer panic");
                            }
                            emit(i, 1)?;
                        }
                        Ok(())
                    },
                    |i| {
                        if failure == 0 && i == 1 {
                            return Err("worker failure".into());
                        }
                        if failure == 1 && i == 1 {
                            panic!("worker panic");
                        }
                        Ok(i)
                    },
                    |i| {
                        if failure == 2 && i == 0 {
                            panic!("publisher panic");
                        }
                        Ok(())
                    },
                )
            });
            assert!(result.is_err() || result.unwrap().is_err());
        }
        assert!(
            run(
                Limits {
                    workers: 1,
                    bytes: 1,
                    jobs: 1
                },
                |emit| emit((), 2),
                |_| Ok(()),
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            run(
                Limits {
                    workers: 1,
                    bytes: 1,
                    jobs: 1
                },
                |emit| emit((), 1),
                |_| Ok(()),
                |_| Err("write failed".into())
            )
            .is_err()
        );
    }
}
