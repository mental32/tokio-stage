<h1 align="center">Stage</h1>
<h1 align="center">Fault-toleranance Framework For Tokio Applications</h1>

## Index

- [Index](#index)
- [Brief](#brief)
- [Usage](#usage)
  - [Stage: The First Degree](#stage-the-first-degree)
  - [Stage: The Second Degree](#stage-the-second-degree)
  - [Stage: The Third Degree](#stage-the-third-degree)

## Brief

Stage is a framework for building fault-tolerant systems within your tokio
runtime. Designed to make self-healing tasks and actors as simple as
possible respecting the cost-benefit tradeoff and allowing you the programmer
to decide how much code you would like to modify for the benefits stage would
provide you.

## Usage

Stage can be employed in three distinct degrees of use to make migrating to the
patterns in this framework as easy as possible for existing microservices that
use tokio such as to service RPC requests or an actix/axum server.

Degrees are sorted from least intrusive to most, requiring the minimal amount
of change possible to the maximum of your existing code respectively. If you
are considering to use stage in an existing app already then you should be
using the first degree and maybe then the second but if you are building a
new application from the ground up then consider the third.

### Stage: The First Degree

Stage when used in the first degree aims to replace the usage of `tokio::spawn`
with a concept called "task group".

A Task Group (or group) is just a collection of tasks spawned to execute one type of
future created from a function. They hold metadata like policy for the
size of the group, when to not restart a task i.e. if it exited succesfully.

Here is some code that spawns one actor task to process messages and respond.

```rust,no_run
#[derive(Debug)]
enum Message {
    Add(usize, usize, tokio::sync::oneshot::Sender<usize>)
}


#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(512);
    let task = tokio::spawn(async move { // 1.
            while let Some(m) = rx.recv().await {
                let _ = match m {
                    Message::Add(n, m, tx) => tx.send(n + m)
                };
            }
        }
    );

    let (r_tx, r_rx) = tokio::sync::oneshot::channel();
    tx.send(Message::Add(1, 2, r_tx)).await.expect("send failure"); // 2.
    let res = r_rx.await.unwrap();
    assert_eq!(res, 3);
}
```

You may find several issues with the code above

1. Only one task was spawned to process messages. What if the future panics and we want to restart it without restarting the entire process?

2. If the future panics, it will drop the receiver which will cause all the many sender halves of the channel to fault when a send operation occurs. send returns an error indicating if the item could be received and it is handled in one of two ways:
   - the sender ignores the result of the send operation, this will lead to abuse of a dangling channel and a silent corruption
   - the sender panics or yeets the error back potentially triggering a cascading panic across (hopefully) the entire application.


Here is the same code using a stage group:

```rust,no_run
use stage::prelude::*;

use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
enum Message {
    Add(usize, usize, tokio::sync::oneshot::Sender<usize>)
}

#[tokio::main]
async fn main() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let rx = Arc::new(Mutex::new(rx));

    let group = stage::group()
        .spawn(move || {
            let rx = Arc::clone(&rx);
            async move {
            while let Some(m) = rx.lock().await.recv().await {
                let _ = match m {
                    Message::Add(n, m, tx) => tx.send(n + m)
                };
            }
        }}
    );

    loop {
        let (ttx, trx) = tokio::sync::oneshot::channel();
        tx.send(Message::Add(1, 2, ttx)).await.unwrap();

        if let Ok(res) = trx.await {
            assert_eq!(res, 3);
            break;
        }
    }
}
```

The changes in this code are:

- we use `stage::group()` and `.spawn()` to construct a group that is capable of supervising a task that is reconstructable from the provided function.
- we've introduced a loop to retry our message since our task could panic but will restart, the sender half of the channel however will remain broken forever.
  - This means our future now has to not break the receiver half of the channel. This can be a non-issue for some cases where the channel abstraction is elsewhere i.e. zmq or a networking abstraction

There is now a new issue in the second code. our task group will indefinitely spawn tasks to run potentially blocking the tokio runtime from shutting down.

The solution is to scope your task block to the resolution of another future:

```rust,no_compile,no_run
group
    .scope(async move {
        loop {
            let (ttx, trx) = tokio::sync::oneshot::channel();
            tx.send(ttx).await.unwrap();

            if let Ok(res) = trx.await {
                assert_eq!(res, 3);
                break;
            }
        }
    })
    .await;
```

Now when our future finishes our group supervisor will abort and will no longer be restarting the tasks.

### Stage: The Second Degree


### Stage: The Third Degree
