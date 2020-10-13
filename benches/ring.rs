#![feature(test)]
extern crate test;

use test::Bencher;
use async_weighted_semaphore::{Semaphore, PoisonError};
use std::sync::Arc;
use futures::executor::{ThreadPool, block_on};
use futures::task::SpawnExt;
use rand::{Rng, SeedableRng};
use rand_xorshift::XorShiftRng;
use futures::future::join_all;

const PARALLELISM: usize = 8;
const ITEMS: usize = 10;
const ITERS: usize = 10000;
const MAX_ACQUIRE: usize = 5;
const TOKENS: usize = (MAX_ACQUIRE - 1) * ITEMS + 1;

fn run_impl(bencher: &mut Bencher) {
    let pool = ThreadPool::builder().pool_size(PARALLELISM).create().unwrap();
    bencher.iter(|| {
        let items = (0..ITEMS)
            .map(|i| Arc::new(Semaphore::new(if i == 0 { TOKENS } else { 0 })))
            .collect::<Vec<_>>();
        block_on(join_all((0..ITEMS).map(|index| {
            let items = items.clone();
            pool.spawn_with_handle(async move {
                let mut rng = XorShiftRng::from_entropy();
                for _ in 0..ITERS {
                    let amount = rng.gen_range(1usize, 1 << MAX_ACQUIRE + 1).trailing_zeros() as usize;
                    match items[index].acquire(amount).await {
                        Ok(guard) => guard.forget(),
                        Err(PoisonError) => break,
                    };
                    items[(index + 1) % ITEMS].release(amount);
                }
                items[(index + 1) % ITEMS].poison();
            }).unwrap()
        })));
    });
}

#[bench]
fn run_weighted(bencher: &mut Bencher) {
    run_impl(bencher);
}