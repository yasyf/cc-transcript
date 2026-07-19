//! A generic single-thread actor owning one engine `E` built on and never leaving its own
//! thread. Every job runs there in FIFO order, so a blocking SQLite call never convoys the
//! other handles and the engine's `!Sync` connection never crosses a thread boundary. The
//! py bindings submit closures that call the sync engine methods and resolve an asyncio
//! future from the completion.
//!
//! Total delivery is the shape's whole point: a job is `FnOnce(Option<&E>)`, so a failed
//! `make` still drains every queued and future job with `None` (the binding's "closed
//! database" arm) until the last sender drops. `stop` runs the queued jobs first, drops the
//! engine (closing the connection), then fires `done`; a leaked handle needs no `Drop` —
//! dropping the sender disconnects the channel, the thread loop ends, and the engine drops
//! on its own thread.

use std::sync::mpsc::{channel, Sender};
use std::thread::{self, JoinHandle};

use crate::sqlite::LedgerError;

pub type Job<E> = Box<dyn FnOnce(Option<&E>) + Send + 'static>;

enum Message<E> {
    Run(Job<E>),
    Stop(Box<dyn FnOnce() + Send + 'static>),
}

pub struct Actor<E> {
    tx: Sender<Message<E>>,
    handle: JoinHandle<()>,
}

impl<E: 'static> Actor<E> {
    pub fn spawn(
        make: impl FnOnce() -> Result<E, LedgerError> + Send + 'static,
        on_open: impl FnOnce(Result<(), LedgerError>) + Send + 'static,
    ) -> Actor<E> {
        let (tx, rx) = channel::<Message<E>>();
        let handle = thread::spawn(move || match make() {
            Ok(engine) => {
                on_open(Ok(()));
                let closer = loop {
                    match rx.recv() {
                        Ok(Message::Run(job)) => job(Some(&engine)),
                        Ok(Message::Stop(done)) => break Some(done),
                        Err(_) => break None,
                    }
                };
                drop(engine);
                if let Some(done) = closer {
                    done();
                }
            }
            Err(error) => {
                on_open(Err(error));
                while let Ok(message) = rx.recv() {
                    match message {
                        Message::Run(job) => job(None),
                        Message::Stop(done) => {
                            done();
                            break;
                        }
                    }
                }
            }
        });
        Actor { tx, handle }
    }

    pub fn submit(&self, job: Job<E>) -> Result<(), Job<E>> {
        self.tx.send(Message::Run(job)).map_err(|err| match err.0 {
            Message::Run(job) => job,
            Message::Stop(_) => unreachable!("submit only sends Run"),
        })
    }

    pub fn stop(self, done: Box<dyn FnOnce() + Send + 'static>) -> JoinHandle<()> {
        let _ = self.tx.send(Message::Stop(done));
        self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel as std_channel;

    fn ok_make(seed: i32) -> impl FnOnce() -> Result<i32, LedgerError> + Send + 'static {
        move || Ok(seed)
    }

    #[test]
    fn runs_jobs_in_submission_order() {
        let (out_tx, out_rx) = std_channel();
        let actor = Actor::spawn(ok_make(7), |_| {});
        for i in 0..100 {
            let out_tx = out_tx.clone();
            assert!(actor
                .submit(Box::new(move |engine: Option<&i32>| {
                    assert_eq!(engine, Some(&7));
                    out_tx.send(i).unwrap();
                }))
                .is_ok());
        }
        drop(out_tx);
        assert_eq!(
            out_rx.iter().collect::<Vec<_>>(),
            (0..100).collect::<Vec<_>>()
        );
        let (done_tx, done_rx) = std_channel();
        actor
            .stop(Box::new(move || done_tx.send(()).unwrap()))
            .join()
            .unwrap();
        done_rx.recv().unwrap();
    }

    #[test]
    fn stop_drains_every_queued_job_before_done() {
        let (out_tx, out_rx) = std_channel();
        let actor = Actor::spawn(ok_make(0), |_| {});
        for i in 0..50 {
            let out_tx = out_tx.clone();
            assert!(actor
                .submit(Box::new(move |_| out_tx.send(i).unwrap()))
                .is_ok());
        }
        let done_tx = out_tx.clone();
        drop(out_tx);
        actor
            .stop(Box::new(move || done_tx.send(-1).unwrap()))
            .join()
            .unwrap();
        assert_eq!(
            out_rx.iter().collect::<Vec<_>>(),
            (0..50).chain(std::iter::once(-1)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn failed_make_drains_every_job_with_none() {
        let (open_tx, open_rx) = std_channel();
        let (job_tx, job_rx) = std_channel();
        let actor = Actor::spawn(
            || Err::<i32, _>(LedgerError::MultipleStatements),
            move |result| open_tx.send(result.is_err()).unwrap(),
        );
        assert!(open_rx.recv().unwrap());
        for i in 0..10 {
            let job_tx = job_tx.clone();
            assert!(actor
                .submit(Box::new(move |engine: Option<&i32>| {
                    job_tx.send((i, engine.is_none())).unwrap()
                }))
                .is_ok());
        }
        drop(job_tx);
        let mut delivered = job_rx.iter().collect::<Vec<_>>();
        delivered.sort();
        assert_eq!(delivered, (0..10).map(|i| (i, true)).collect::<Vec<_>>());
        let (done_tx, done_rx) = std_channel();
        actor
            .stop(Box::new(move || done_tx.send(()).unwrap()))
            .join()
            .unwrap();
        done_rx.recv().unwrap();
    }
}
