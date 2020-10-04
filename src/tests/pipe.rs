use crate::{Semaphore, AcquireError};
use std::sync::Arc;
use futures_test::std_reexport::collections::VecDeque;
use async_std::sync::Mutex;
use std::io::{BufReader, BufRead};
use std::{mem, thread};
use futures::executor::block_on;
use rand::{thread_rng, Rng};



#[derive(Clone)]
struct Pipe {
    ready: Arc<Semaphore>,
    free: Arc<Semaphore>,
    buffer: Arc<Mutex<VecDeque<u8>>>,
}

struct ReaderInner(Pipe);

struct WriterInner(Pipe);

#[derive(Clone)]
pub struct Reader {
    inner: Arc<ReaderInner>,
}

#[derive(Clone)]
pub struct Writer {
    inner: Arc<WriterInner>
}

fn pipe(capacity: usize) -> (Writer, Reader) {
    let ready = Arc::new(Semaphore::new(0));
    let free = Arc::new(Semaphore::new(capacity));
    let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));
    let pipe = Pipe { ready, free, buffer };
    let reader = Arc::new(ReaderInner(pipe.clone()));
    let writer = Arc::new(WriterInner(pipe.clone()));
    (Writer { inner: writer }, Reader { inner: reader })
}

impl Reader {
    async fn read_exact(&self, buf: &mut [u8]) -> usize {
        assert!(buf.len() < self.inner.0.buffer.lock().await.capacity());
        let total = if let Ok(guard) = self.inner.0.ready.acquire(buf.len()).await {
            guard.forget();
            let mut lock = self.inner.0.buffer.lock().await;
            assert!(buf.len() <= lock.len());
            for b in buf.iter_mut() {
                *b = lock.pop_front().unwrap();
            }
            buf.len()
        } else {
            let mut lock = self.inner.0.buffer.lock().await;
            let length = lock.len().min(buf.len());
            for (i, b) in lock.drain(..length).enumerate() {
                buf[i] = b;
            }
            length
        };
        self.inner.0.free.release(total);
        total
    }
}

impl Writer {
    async fn write_all(&self, buf: &[u8]) -> Result<(), AcquireError> {
        assert!(buf.len() < self.inner.0.buffer.lock().await.capacity());
        self.inner.0.free.acquire(buf.len()).await?.forget();
        let mut lock = self.inner.0.buffer.lock().await;
        lock.extend(buf.iter().cloned());
        mem::drop(lock);
        self.inner.0.ready.release(buf.len());
        Ok(())
    }
}

impl Drop for ReaderInner {
    fn drop(&mut self) {
        self.0.free.poison();
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        self.0.ready.poison();
    }
}

#[test]
fn test_pipe() {
    for _i in 0..100 {
        test_pipe_impl();
    }
}

fn test_pipe_impl() {
    let threads = 10;
    let iters = 1000;
    let (w, r) = pipe(20);
    for _ in 0..threads {
        let w = w.clone();
        thread::spawn(move || block_on(async {
            for _ in 0..iters {
                let n = thread_rng().gen_range(0, 1000);
                w.write_all(format!("{}\n", n).as_ref()).await.unwrap();
            }
        }));
    }
    mem::drop(w);
    block_on(async {
        let mut result = Vec::new();
        loop {
            let mut buf = vec![0u8; thread_rng().gen_range(0, 10)];
            let n = r.read_exact(&mut buf).await;
            result.extend_from_slice(&buf[0..n]);
            if n != buf.len() {
                break;
            }
        }
        let mut lines = 0;
        for line in BufReader::new(&mut result.as_slice()).lines() {
            line.unwrap().parse::<usize>().unwrap();
            lines += 1;
        }
        assert_eq!(threads * iters, lines);
    });
}