//! Bounded per-connection send queues. A peer that stops reading must not
//! make the server buffer without limit: when its queue is full, the
//! connection is closed instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, Notify};

/// Messages waiting for one WebSocket connection.
pub const QUEUE_CAPACITY: usize = 1024;

#[derive(Default)]
struct Closer {
    closed: AtomicBool,
    notify: Notify,
}

impl Closer {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// The sending side, kept in the server's connection maps.
pub struct Outbox<T> {
    tx: mpsc::Sender<T>,
    closer: Arc<Closer>,
}

impl<T> Clone for Outbox<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), closer: self.closer.clone() }
    }
}

impl<T> Outbox<T> {
    /// Queues `msg`; a full queue closes the connection.
    pub fn send(&self, msg: T) -> Result<(), ()> {
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("Connection send queue full; closing the connection");
                self.closer.close();
                Err(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }

    /// Ends the connection (e.g. its user was deactivated).
    pub fn close(&self) {
        self.closer.close();
    }
}

/// The receiving side, read by the connection's writer task.
pub struct Inbox<T> {
    rx: mpsc::Receiver<T>,
    closer: Arc<Closer>,
}

impl<T> Inbox<T> {
    /// The next message; None once the connection is to be closed.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            // Registered before the flag is read, so a close in between is seen.
            let notified = self.closer.notify.notified();
            if self.closer.closed.load(Ordering::SeqCst) {
                return None;
            }
            tokio::select! {
                _ = notified => continue,
                msg = self.rx.recv() => return msg,
            }
        }
    }
}

#[cfg(test)]
impl<T> Inbox<T> {
    pub fn rx_try(&mut self) -> Option<T> {
        self.rx.try_recv().ok()
    }
}

pub fn channel<T>() -> (Outbox<T>, Inbox<T>) {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let closer = Arc::new(Closer::default());
    (Outbox { tx, closer: closer.clone() }, Inbox { rx, closer })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_queue_closes_the_connection() {
        let (tx, mut rx) = channel::<u32>();
        for i in 0..QUEUE_CAPACITY as u32 {
            tx.send(i).unwrap();
        }
        assert!(tx.send(0).is_err());
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn close_wakes_a_waiting_reader() {
        let (tx, mut rx) = channel::<u32>();
        tx.send(7).unwrap();
        assert_eq!(rx.recv().await, Some(7));
        let reader = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        tx.close();
        assert_eq!(reader.await.unwrap(), None);
    }
}
