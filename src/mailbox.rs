use std::collections::VecDeque;
use std::ops::Deref;

use tokio::sync::oneshot;

struct Mailbox<T> {
    queue: VecDeque<T>,
    pending: VecDeque<oneshot::Sender<T>>,
}

impl<T> Mailbox<T> {
    fn push(&mut self, mut t: T) {
        loop {
            match self.pending.pop_front() {
                Some(tx) => match tx.send(t) {
                    Ok(()) => break,
                    Err(t2) => t = t2,
                },
                None => {
                    self.queue.push_back(t);
                    break;
                }
            }
        }
    }

    fn pop(&mut self, tx: oneshot::Sender<T>) {
        match self.queue.pop_front() {
            Some(t) => match tx.send(t) {
                Ok(()) => (),
                Err(t) => self.queue.push_front(t),
            },
            None => self.pending.push_back(tx),
        }
    }
}

#[derive(Debug)]
enum Message<T> {
    Push(T),
    Pop(tokio::sync::oneshot::Sender<T>),
}

#[derive(Debug)]
#[repr(transparent)]
struct MailboxChannel<T>(tokio::sync::mpsc::Sender<Message<T>>);

impl<T> Deref for MailboxChannel<T> {
    type Target = tokio::sync::mpsc::Sender<Message<T>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Clone for MailboxChannel<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxChannel<T> {
    async fn pop(&self) -> T {
        let (tx, rx) = tokio::sync::oneshot::channel();

        match self.deref().send(Message::Pop(tx)).await {
            Ok(()) => rx.await.expect("mailbox actor has dropped channel"),
            Err(_) => panic!("mailbox actor channel is closed"),
        }
    }

    async fn push(&self, t: T) {
        match self.deref().send(Message::Push(t)).await {
            Ok(()) => (),
            Err(_) => panic!("mailbox actor channel is closed"),
        }
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub struct MailboxSender<T>(MailboxChannel<T>);

impl<T> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxSender<T> {
    pub async fn send(&self, message: T) {
        self.0.push(message).await
    }
}

#[derive(Debug)]
pub struct MailboxReceiver<T>(MailboxChannel<T>);

impl<T> Clone for MailboxReceiver<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxReceiver<T> {
    #[inline]
    pub async fn recv(&self) -> T {
        self.0.pop().await
    }
}

/// Create a mailbox channel that can't get closed accidentally and will preserve messages.
///
/// # Example
///
/// ```rust
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicUsize, Ordering};
/// use std::time::Duration;
///
/// use tokio::sync::Barrier;
///
/// struct State {
///     counter: AtomicUsize,
///     barrier: Barrier,
///     rx: stage::MailboxReceiver<()>
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let (tx, rx) = stage::mailbox(512);
///     let state = State {
///         counter: AtomicUsize::new(0),
///         barrier: Barrier::new(2),
///         rx,
///     };
///
///     let state = Arc::new(state);
///     let state2 = Arc::clone(&state);
///     let group = stage::group()
///         .spawn(move || {
///         let state = Arc::clone(&state2);
///         async move {
///             loop {
///                 let () = state.rx.recv().await;
///                 if state.counter.fetch_add(1, Ordering::SeqCst) == 499 {
///                     state.barrier.wait().await;
///                     break;
///                 }
///             }
///         }
///     });
///
///     for i in 0..500 {
///         tx.send(()).await;
///     }
///     state.barrier.wait().await;
///     assert_eq!(state.counter.load(Ordering::SeqCst), 500);
/// }
/// ```
#[inline]
pub fn mailbox<T: Send + 'static>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let channel = MailboxChannel(tx);

    let mbox = Mailbox {
        queue: VecDeque::with_capacity(capacity),
        pending: VecDeque::new(),
    };

    tokio::spawn(async move {
        let mut mbox = mbox;

        while let Some(m) = rx.recv().await {
            match m {
                Message::Push(t) => mbox.push(t),
                Message::Pop(tx) => mbox.pop(tx),
            }
        }
    });

    (MailboxSender(channel.clone()), MailboxReceiver(channel))
}

#[cfg(test)]
mod test {
    #[tokio::test]
    async fn test_pos_mbox() {
        let (tx, rx) = super::mailbox(4);

        tx.send(1).await;
        assert_eq!(rx.recv().await, 1);

        tx.send(2).await;
        tx.send(3).await;
        tx.send(4).await;

        assert_eq!(rx.recv().await, 2);
        assert_eq!(rx.recv().await, 3);
        assert_eq!(rx.recv().await, 4);
    }
}
