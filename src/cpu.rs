use anyhow::{Context, Result};
use rayon::ThreadPool;
use rayon::prelude::*;
use tokio::runtime::{Handle, RuntimeFlavor};

pub fn pool(threads: usize) -> Result<ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .context("cannot start the worker threads")
}

pub fn map<T: Sync, R: Send>(
    pool: &ThreadPool,
    items: &[T],
    f: impl Fn(&T) -> R + Sync + Send,
) -> Vec<R> {
    pool.install(|| items.par_iter().map(f).collect())
}

pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_keeps_order() {
        let items: Vec<u32> = (0..100).collect();
        let out = map(&pool(7).unwrap(), &items, |x| x * 2);
        assert_eq!(out, items.iter().map(|x| x * 2).collect::<Vec<_>>());
        assert!(map(&pool(0).unwrap(), &Vec::<u32>::new(), |x| *x).is_empty());
    }

    #[test]
    fn blocking_runs_inside_and_outside_a_runtime() {
        assert_eq!(blocking(|| 1), 1);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(async { blocking(|| 2) }), 2);
        let spawned = runtime.block_on(async { tokio::spawn(async { blocking(|| 3) }).await });
        assert_eq!(spawned.unwrap(), 3);
        let current = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(current.block_on(async { blocking(|| 4) }), 4);
    }
}
