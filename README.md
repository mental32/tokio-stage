<h1 align="center">Stage</h1>
<h1 align="center">Fault-toleranance Framework For Tokio Applications</h1>

## Index

- [Index](#index)
- [Brief](#brief)
- [Usage](#usage)
  - [Stage: The First Degree](#stage-the-first-degree)
    - [Task Group (group)](#task-group-group)
  - [Stage: The Second Degree](#stage-the-second-degree)
    - [Mailbox (reliable channel)](#mailbox-reliable-channel)
      - [The Problem](#the-problem)
      - [The Solution(s)](#the-solutions)
    - [Supervision Tree's](#supervision-trees)
  - [Stage: The Third Degree](#stage-the-third-degree)
- [bottom of the page](#bottom-of-the-page)

## Brief

Stage is a framework for building fault-tolerant systems within your tokio
runtime. Designed to make self-healing tasks and actors as simple as
possible respecting the cost-benefit tradeoff and allowing you the programmer
to decide how much code they would like to modify for the benefits stage would
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

#### Task Group (group)

A Task Group (or group) is just a collection of tasks spawned to execute one type of
future created from a function. They hold metadata like policy for the
size of the group, when to not restart a task i.e. if it exited succesfully.

Here is some code that spawns one actor task to process messages and respond.

```rust
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

```rust
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

There is now a new potential issue in the code. the task group will indefinitely spawn tasks to run and may block the tokio runtime from shutting down.

The solution is to scope your task block to the resolution of another future:

```rust,no_run
# let (tx, rx) = tokio::sync::mpsc::channel(1);
# let group: stage::group::Group = unimplemented!();
# #[derive(Debug)]
# enum Message {
#     Add(usize, usize, tokio::sync::oneshot::Sender<usize>)
# }
# async { 
group
    .scope(async move {
        loop {
            let (ttx, trx) = tokio::sync::oneshot::channel();
            tx.send(Message::Add(1, 2, ttx)).await.unwrap();

            if let Ok(res) = trx.await {
                assert_eq!(res, 3);
                break;
            }
        }
    })
    .await;
# };
```

Now when our future finishes our group supervisor will abort and will no longer be restarting the tasks.

Here are some more things you can do with a group:

* upscale - dynamically upscale the amount of futures running and managed by the supervisor task
* shutdown - gracefully shutdown the supervisor task and receive control of all the task joinhandles
* stats - `stat_borrow(&self)` and `stat_borrow_and_update(&mut self)` allow you to inspect statistics published by the supervisor such as the amount of tasks running, the amount that have failed, the amount of iterations the supervisor loop has made processing tasks and messages.

### Stage: The Second Degree

Stage used in the second degree focuses on actor control flow primitives, 
unbreakable channels, and other patterns to suppliment the usage of task
groups.

#### Mailbox (reliable channel)

Here we will introduce the `stage::mailbox` and explore what problem
it solves and how to use it.

##### The Problem

In the last section when we replaced `tokio::spawn` with a task group we
discovered an issue. the tokio mpsc channel will close the whole channel
if either all sender halves are dropped or the one receiver half is dropped.

This is an issue because if our future panics then the receiver will get
dropped, closing the whole channel, rending the future owning a sender half
as fragile an unreliable as the task inside the task group.

Chances are the code was written with a "crash first" mentality so that
the slightest issue will eventually lead to the container orchestrator to
restart the process or you have decided to be more "resilliant" and ignore
small issues that aren't fatal; opting to perhaps issue an alert to your
monitoring infrastructure which leads to an operator manually restarting later.

neither of these "solutions" are exactly ideal so what _could_ you do about
this?

1. wrap your receiver half in an `Arc` and `Mutex`. This makes the receiver
   half almost undroppable, recloanable, and preserves a single reader property.

2. link your task groups together so that you have a `one_for_all` supervisor
   strategy. effectively hard rebooting your program inplace without the
   process exiting.

The second suggestion we will explore later with the concept of supervision
tree's. At this moment you can try to do it yourself but you'll find that there
is a non-trivial amount of boilerplate and logic needed to be embedded into your
app.

The first suggestion is what we did with the code in the last section, here
it is so you can remind yourself:

```rust
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
    let rx = Arc::new(Mutex::new(rx)); // <=== Here we make our receiver half "reliable"

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

    group
        .scope(async move {
            loop { // <=== here we have to retry the operation until it succeedes
                let (ttx, trx) = tokio::sync::oneshot::channel();
                tx.send(Message::Add(1, 2, ttx)).await;

                if let Ok(res) = trx.await {
                    assert_eq!(res, 3);
                    break;
                }
            }
        })
        .await;
}
```

##### The Solution(s)

Let's reiterate what we're trying to solve here:

* premature closure - if the receiver gets dropped it shuts down the whole
  channel affecting senders

* message loss - if the task crashes after it's read a message but before it
  could do work then this message is now lost. you'll need to decide if it's
  worth it to track retransmitting messages to ensure the operation gets
  completed.

This is actually trivially solvable using existing technology today:

1. journaling and transactions - Redis, or any database system; marshal your
   messages into the store and then the worker will read and process where it
   last had a checkpoint

2. message brokering - ZeroMQ, ActiveMQ, Kafka; these will all provide reliable
   messaging and answers message persistance

The only downside of this is that you have resort to out-of-process message
passing and every time you write to a queue or read a message from it you
have to serialize and deserialize the contents to your type which has a
non-zero cost.

The third solution offered here is the mailbox. (`stage::mailbox`)

A mailbox is just a queue (`std::collections::VecDeque`) managed by an actor.
sending and receiving and modeled as pushing and popping data on the queue.
This also allows you to preserve the bits of the data without serializing or
deserializing it for writes and reads.

Let's modify the code above to use a mailbox:

```rust
use stage::prelude::*;

use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
enum Message {
    Add(usize, usize, tokio::sync::oneshot::Sender<usize>)
}

#[tokio::main]
async fn main() {
    // - let (tx, rx) = tokio::sync::mpsc::channel(1);
    // - let rx = Arc::new(Mutex::new(rx)); // <=== Here we make our receiver half "reliable"
    let (tx, rx) = stage::mailbox(1);

    let group = stage::group()
        .spawn(move || {
            // - let rx = Arc::clone(&rx);
            let rx = rx.clone();
            async move {
            // while let Some(m) = rx.lock().await.recv().await {
               loop {
                   let m = rx.recv().await;
                   let _ = match m {
                       Message::Add(n, m, tx) => tx.send(n + m)
                   };
               }
            }
        }
    );

    group
        .scope(async move {
            loop { // <=== here we have to retry the operation until it succeedes
                let (ttx, trx) = tokio::sync::oneshot::channel();
                // - tx.send(Message::Add(1, 2, ttx)).await.unwrap();
                tx.send(Message::Add(1, 2, ttx)).await;

                if let Ok(res) = trx.await {
                    assert_eq!(res, 3);
                    break;
                }
            }
        })
        .await;
}
```

#### Supervision Tree's

TODO

### Stage: The Third Degree

Stage in the third degree is similar to [bastion] or [actix]. Here we aim to
provide the user with a declarative trait-based actor framework and a runtime
to manage and supervise task groups, supervision trees, and the spawned actors
themselves.

In the previous sections we spoke about using `stage::group()` to establish a
"task" primitive and rules for replication and redundancy in a way that resembles
erlang style supervision trees, then we spoke about using extra primitives to
enable programming of specific structured concurrency patterns for
fault-tolerance here you can mimick erlang supervision trees in their entirety.

[bastion]: https://docs.rs/bastion/
[actix]: https://docs.rs/actix

## bottom of the page
