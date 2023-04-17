use std::collections::VecDeque;
use std::ops::Deref;

use tokio::sync::{mpsc, oneshot};

struct Mailbox<T> {
    message_queue: VecDeque<T>,
    pending_readers: VecDeque<oneshot::Sender<T>>,
}

impl<T> Mailbox<T> {
    fn push(&mut self, mut t: T) {
        loop {
            match self.pending_readers.pop_front() {
                Some(tx) => match tx.send(t) {
                    Ok(()) => break,
                    Err(t2) => t = t2,
                },
                None => {
                    self.message_queue.push_back(t);
                    break;
                }
            }
        }
    }

    fn pop(&mut self, tx: oneshot::Sender<T>) {
        match self.message_queue.pop_front() {
            Some(t) => match tx.send(t) {
                Ok(()) => (),
                Err(t) => self.message_queue.push_front(t),
            },
            None => self.pending_readers.push_back(tx),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Message<T> {
    Push(T),
    Pop(oneshot::Sender<T>),
}

#[derive(Debug)]
#[repr(transparent)]
struct MailboxChannel<T>(mpsc::Sender<Message<T>>);

impl<T> Deref for MailboxChannel<T> {
    type Target = mpsc::Sender<Message<T>>;

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
        let (tx, rx) = oneshot::channel();

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

/// Used to send/push messages to the mailbox.
#[derive(Debug)]
#[repr(transparent)]
pub struct MailboxSender<T>(MailboxChannel<T>);

impl<T> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxSender<T> {
    pub(crate) fn into_raw(self) -> mpsc::Sender<Message<T>> {
        self.0 .0
    }

    pub(crate) fn from_raw(tx: mpsc::Sender<Message<T>>) -> Self {
        Self(MailboxChannel(tx))
    }

    /// Push a message to the queue allowing [`MailboxReceiver`]s to receive it.
    #[inline]
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
    pub async fn send(&self, message: T) {
        self.0.push(message).await
    }
}

/// Used to receive/pop messages from the mailbox.
#[derive(Debug)]
pub struct MailboxReceiver<T>(MailboxChannel<T>);

impl<T> Clone for MailboxReceiver<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> MailboxReceiver<T> {
    /// Receive a message from the queue
    #[inline]
    #[cfg_attr(feature = "backtrace", async_backtrace::framed)]
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
///     rx: tokio_stage::MailboxReceiver<()>
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let (tx, rx) = tokio_stage::mailbox(512);
///     let state = State {
///         counter: AtomicUsize::new(0),
///         barrier: Barrier::new(2),
///         rx,
///     };
///
///     let state = Arc::new(state);
///     let state2 = Arc::clone(&state);
///     let group = tokio_stage::group()
///          .spawn(move || {
///          let state = Arc::clone(&state2);
///          async move {
///              loop {
///                  let () = state.rx.recv().await;
///                  if state.counter.fetch_add(1, Ordering::SeqCst) == 499 {
///                      state.barrier.wait().await;
///                      break;
///                  }
///
///              }
///          }
///      });
///
///      for i in 0..500 {
///          tx.send(()).await;
///      }
///
///      state.barrier.wait().await;
///      assert_eq!(state.counter.load(Ordering::SeqCst), 500);
///      group.exit(std::time::Duration::from_secs(1)).await;
///  }
/// ```
#[inline]
#[track_caller]
pub fn mailbox<T: Send + 'static>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    let (tx, mut rx) = mpsc::channel(capacity);
    let channel = MailboxChannel(tx);

    let mbox = Mailbox {
        message_queue: VecDeque::with_capacity(capacity),
        pending_readers: VecDeque::new(),
    };

    crate::task::spawn(
        async move {
            let mut mbox = mbox;

            while let Some(m) = rx.recv().await {
                match m {
                    Message::Push(t) => mbox.push(t),
                    Message::Pop(tx) => mbox.pop(tx),
                }
            }
        },
        crate::task::TaskKind::Worker,
    );

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
