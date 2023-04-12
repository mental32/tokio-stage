Stage
Self-healing and Fault-tolerance library for Tokio Applications

- [Brief](#brief)
  - [Features](#features)
  - [Alternatives](#alternatives)
  - ["Why Use Stage?"](#why-use-stage)
  - [Stage: Tutorial](#stage-tutorial)
    - [Stage: The First Degree](#stage-the-first-degree)
      - [Code snippet 1.1](#code-snippet-11)
      - [Code snippet 1.2](#code-snippet-12)
      - [Code snippet 1.3](#code-snippet-13)
      - [Conclusion](#conclusion)
    - [Stage: The Second Degree](#stage-the-second-degree)
      - [Mailbox](#mailbox)
      - [Code snippet 2.1](#code-snippet-21)
      - [Graceful Shutdown](#graceful-shutdown)
      - [Code snippet 2.2](#code-snippet-22)
      - [Supervision Trees](#supervision-trees)
      - [Code snippet 2.3](#code-snippet-23)
    - [Stage: The Third Degree](#stage-the-third-degree)
- [bottom of the page](#bottom-of-the-page)

## Brief

Stage is a library that enables Rust code using Tokio to become more robust,
gain self-healing, and fault-tolerant properties at the task-level.

Respecting the cost-benefit tradeoff stage comes in three degrees allowing the
programmer to decide on how much of their code they would like to rewrite for
the benefits stage would provide.

### Features

* groups: making tokio tasks restartable and scalable
* mailbox: in-memory channel with reliable message delivery guarantees
* supervion: composing groups together to enable erlang-style [supervision-trees]

[supervision-trees]: https://erlang.org/documentation/doc-4.9.1/doc/design_principles/sup_princ.html

### Alternatives

* [bastion] - Highly-available Distributed Fault-tolerant Runtime
* [zestors] - A fast and flexible actor-framework for building fault-tolerant Rust applications
* [ractor] - A pure-Rust actor framework. Inspired from Erlang's gen_server
* [actix] -  Actor framework for Rust.
* [lunatic] - Lunatic is an Erlang-inspired runtime for WebAssembly

Not to mention the infinite list of actor library/framework crates that have
been published and abandoned floating around on crates.io

### "Why Use Stage?"

1. Arrogantly noninvasive.
   Alternate implementations will make themselves the center of your logic
   forcing you to go all in or not at all. Stage knows the tokio runtime
   is already the core pillar of your program runtime. all requirements are
   basic Rust language features e.g. closures and futures. and Tokio.

2. Dead simple.
   Stage places a focus and emphasis on familiarity and ergonomics. There's not
   much you need to do in order to start using benefiting from self-healing
   code

3. Strong emphasis on the Tokio ecosystem.
   Stage is designed as a set of abstractions that are simply reusing tokio
   features and provide compatability with other tokio ecosystem projects like
   [tower], [console], and [tracing]

### Stage: Tutorial

Stage can be employed in three distinct degrees of use to make migrating to the
patterns in this framework as easy as possible for existing microservices that
use tokio such as to service RPC requests or an actix/axum server.

Degrees are sorted from least intrusive to most, requiring the minimal amount
of change possible to the maximum of your existing code respectively. If you
are considering to use stage in an existing app already then you should be
using the first degree and maybe then the second but if you are building a
new application from the ground up then consider the third.

#### Stage: The First Degree

Stage when used in the first degree aims to replace the usage of `tokio::spawn`
with Task Groups (group for short.) via `stage::spawn` or `stage::group().spawn(/* ... */)`

A "task group" is conceptually speaking a set of tasks managed by a supervisor
that is responsible for restarting, upscaling, and aborting/shutting down the
tasks. The erlang analog here is a supervisor with a `:simple_one_for_one`
strategy.

Let us walk through an exmple of porting some existing code from using
`tokio::spawn` to `stage::spawn`.

##### Code snippet 1.1

```rust
#[derive(Debug)]
enum Message {
    Add(usize, usize, tokio::sync::oneshot::Sender<usize>)
}


#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    // 1. spawn the worker task to process messages.
    let task = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let _ = match m {
                    Message::Add(n, m, tx) => tx.send(n + m)
                };
            }
        }
    );

    let (r_tx, r_rx) = tokio::sync::oneshot::channel();
    // 2. send a message to the worker for the processing
    tx.send(Message::Add(1, 2, r_tx)).await.expect("send failure");
    // 3. block on and wait for the reply
    let res = r_rx.await.unwrap();
    assert_eq!(res, 3);
}
```

This is an overly simplistic example but you can easily imagine code in the
real world that maps to essentially the pattern above of:

1. create an mpsc channel to talk to the
2. spawn a task that runs a future to receive messages and process them
3. later use the sender half of the channel to send work to be done
4. wait for the result of the done work

There are several things that can fail in the code above:

1. sending the message to the worker can fail if the receiver gets dropped
2. waiting for the response can fail if the oneshot sender gets dropped by the
   worker
3. both of these can happen due to bad logic or an exception in the worker
   future that causes the task to fail.

Now lets see the code if we use a `stage::group` and address the issues.

##### Code snippet 1.2

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
    // 1. create the channel same as before
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    // 2. in order to make the channel "reliable" avoiding closure by dropping we must make ownership shared, and access exclusive.
    let rx = Arc::new(Mutex::new(rx));

    // 3. analog to tokio::spawn here. except you spawn with a group and you must provide a function
    //    `F: Fn() -> Fut + Clone` and `Fut: Future<Output = ()> + 'static + Send` this constructor
    //    function is the core of what allows tasks to be restartable. first the future itself must
    //    be recreatable!
    let group = stage::group()
        .spawn(move || {
            let rx = Arc::clone(&rx);
            async move {
               while let Some(m) = rx.lock().await.recv().await {
                   let _ = match m {
                       Message::Add(n, m, tx) => tx.send(n + m)
                   };
               }
           }
        }
    );

    // 4. send the message and block for the response.
    //    a. if sending fails then the supervisor is dead
    //    b. if waiting for the response fails then the task dropped the transmitter
    //    the task will be restarted so we can simply loop and retry the operation
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

This new snippet is now much more reliable. the task will be restarted if it
fails and our adding operation will be retried until it succeedes. great success!

One small issue. `#[tokio::main]` will pass our async block to the runtime
`block_on` function. which does not abort or shutdown tasks that have been
spawned. and the runtimes drop code will wait indefinitely for the spawned tasks
to finish normally or panic.

The solution is to use `group.scope` to shutdown the supervisor and the
tasks upon the completion of another future:

##### Code snippet 1.3

```rust,no_compile,no_run
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
```

now when this future is completed the supervisor will be signalled to shutdown
itself and the spawned tasks.

##### Conclusion

We have briefly touched upon `stage::group` and how you can migrate code using
`tokio::spawn` and harden your channels to a more fault-tolerant pattern.

groups are more powerful than you think! here are just some of the things
you can do with a group that we did not touch upon:

* Worker Upscaling - dynamically upscale the amount of futures running and managed by the supervisor task
* Supervisor Statistics - `group.stat_borrow(&self)` and `group.stat_borrow_and_update(&mut self)` allow you to inspect statistics published by the supervisor such as the amount of tasks running, the amount that have failed, the amount of iterations the supervisor loop has made processing tasks and messages.

#### Stage: The Second Degree

Stage used in the second degree focuses on synchronization primitives,
reliable channels, and building supervision trees.

##### Mailbox

In the previous section we ran into the issue of what to do when the channel
to a worker breaks when it is dropped. the naive solution is to wrap it in
an `Arc` and `Mutex`; this solved the issue but isn't ideal. we'll explore
solutions in the following section and introduce the concept of a "mailbox"
(`stage::mailbox`)

So we cant use a plain channel since its fragile and will break. and we dont
want to wrap it in an arc+mutex since that's quite innefficient. How can we
solve this?

The answer is this problem is already solved. as long as we write the message
to a store or queue that the worker task can asynchronously read from then
we move the availability guarentees from the tasks to the 3rd party.

1. journal and atomic read+write - Redis or any modern database solution; write
   the message as a row or entry in a document and the worker task tracks what
   it last read and therefore knows what to read next.

2. message brokering - ZeroMQ, ActiveMQ, Kafka; these are all industry grade
   solutions for ensuring messages that get sent, will get received. designed
   specifically for message passing.

The only downside of this is that you have resort to out-of-process message
passing and every time you write to a queue or read a message from it you
have to serialize and deserialize the contents to your type which has a
non-zero cost.

The third solution offered here is the mailbox. (`stage::mailbox`)

A mailbox is just a queue (`std::collections::VecDeque`) managed by an actor.
sending and receiving and modeled as pushing and popping data on the queue.
This also allows you to preserve the bits of the data without serializing or
deserializing it for writes and reads.

We'll modify the code from the previous section to use a mailbox:

##### Code snippet 2.1

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
    // Deleted: let (tx, rx) = tokio::sync::mpsc::channel(1);
    // Deleted: let rx = Arc::new(Mutex::new(rx));
    let (tx, rx) = stage::mailbox(1); // New!

    let group = stage::group()
        .spawn(move || {
            // Deleted: let rx = Arc::clone(&rx);
            let rx = rx.clone();
            async move {
            // Deleted: while let Some(m) = rx.lock().await.recv().await {
               loop { // New!
                   let m = rx.recv().await; // New!
                   let _ = match m {
                       Message::Add(n, m, tx) => tx.send(n + m)
                   };
               }
            }
        }
    );

    group
        .scope(async move {
            loop {
                let (ttx, trx) = tokio::sync::oneshot::channel();
                // Deleted: tx.send(Message::Add(1, 2, ttx)).await.unwrap();
                tx.send(Message::Add(1, 2, ttx)).await; // New!

                if let Ok(res) = trx.await {
                    assert_eq!(res, 3);
                    break;
                }
            }
        })
        .await;
}
```

##### Graceful Shutdown

A graceful shutdown is an essential aspect of any robust software system. It
refers to the process of shutting down a system or application in an orderly
and controlled manner, ensuring that all ongoing tasks are completed,
resources are released, and no data is lost or corrupted.

There is no default way to perform a graceful shutdown or abort of a future or
task running in Tokio today. but Stage acknowledges the importance of such
functionality and provides a thin wrapper `stage::graceful_shutdown` so that
users can mark their futures as "droppable" and have a shutdown function run
when the task is signalled to shutdown.

##### Code snippet 2.2

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::future::Future;
use std::time::Duration;

// 1. The task_a function takes a tokio::sync::mpsc::Sender as a parameter and
//    returns a future that represents the task. The task sends two messages (1 and 2) over the channel.
fn task_a(tx: tokio::sync::mpsc::Sender<usize>) -> impl Future<Output = ()> {
    let tx_2 = tx.clone();

    // 2. Inside task_a, a worker_fut future is created, which asynchronously
    //    sends the value 1 over the channel and then enters a pending state.
    //    The pending state simulates a long-running operation that doesn't complete on its own.
    let worker_fut = async move {
        tx.send(1).await;
        std::future::pending::<()>().await;
    };

    // 3. A shutdown_fut future is created using the stage::graceful_shutdown
    //    function. This future wraps the worker_fut and specifies a shutdown behavior
    //    using a closure. In this case, the shutdown behavior sends the value 2 over
    //    the channel when the shutdown is triggered.
    let shutdown_fut = stage::graceful_shutdown(worker_fut, move |_worker_fut_pin| {
        async move {
            tx_2.send(2).await;
        }
    });

   // 4. The task_a function returns an async block that awaits the completion
   //    of the shutdown_fut future.
    async move {
        let _: Option<()> = shutdown_fut.await;
    }
}

#[tokio::main]
async fn main() {
    // 5. In the main function, a tokio::sync::mpsc channel is created for
    //    communication between the task and the main function.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    // 6. A task group is created using the stage::group function and spawns
    //    the task_a with a clone of the sender part of the channel.
    let group = stage::group()
        .spawn(move || task_a(tx.clone()));

    // 7. The group.scope function is called with an async block that awaits the 
    //    first value from the channel. The scope function executes the tasks in the
    //    group and blocks until the provided future completes. In this case, it
    //    will complete when it receives the first value (1) from the channel.
    let t = group.scope(async { return rx.recv().await; }).await;
    // 7.1 The first value received by the main function is 1.
    assert_eq!(t, Some(1));
    // 7.2 The second value received by the main function is 2, which is sent during the graceful shutdown.
    assert_eq!(rx.recv().await, Some(2));
    // 7.3 The third value received by the main function is None, indicating that the channel has been closed and no more values will be sent.
    assert_eq!(rx.recv().await, None);
}
```

##### Supervision Trees

Supervision trees in Stage are inspired by Erlang/OTP supervision trees
and will use task groups as the nodes.

A supervision tree is a hierarchical structure that allows you to manage a set
of related tasks, where each node in the tree represents a task group. The
tree is constructed with different strategies that dictate how the tree should
react to failures within its nodes. This enables you to isolate faults, manage
dependencies, and automatically restart failed tasks, ultimately making your
application more resilient.

##### Code snippet 2.3

```rust
async fn task_a() {
    std::future::pending().await
}

fn group_task_a() -> stage::Group {
    stage::group().spawn(task_a)
}

#[tokio::main]
async fn main() {
    let sv1 = stage::supervisor(stage::SupervisorStrategy::OneForOne);
    let child1 = sv1.add_child(group_task_a()).await;
    let sv2 = stage::supervisor(stage::SupervisorStrategy::OneForAll);
    let child2 = sv2.add_child(group_task_a()).await;
    let child3 = sv1.add_child(sv2).await;
}
```

```text
           sv1
        /   |   
  child1  sv2
            |
        child2
```

The supervision tree consists of the following components:

1. sv1 is a supervisor created using the stage::supervisor function with the
   OneForOne strategy. It means that if a child node fails, only the failed
   child will be restarted.

2. child1 is a task group created by group_task_a and added to sv1 as a child.

3. sv2 is another supervisor created using the stage::supervisor function with
   the OneForAll strategy. It means that if a child node fails, all children
   under this supervisor will be restarted. sv2 is added as a child of sv1.

4. child2 is a task group created by group_task_a and added to sv2 as a child.

In this supervision tree, sv1 is the root node, and sv2 is an intermediate
node. The tree provides a fault-tolerant structure, allowing tasks to be
monitored and restarted depending on the chosen supervisor strategy. In this
example, the two supervisors (sv1 and sv2) apply different restart strategies
for their respective child nodes.

#### Stage: The Third Degree

## bottom of the page

[bastion]: https://docs.rs/bastion/latest/bastion/
[ractor]: https://github.com/slawlor/ractor
[zestors]: https://github.com/Zestors/zestors
[actix]: https://github.com/actix/actix
[lunatic]: https://github.com/lunatic-solutions/lunatic

[tower]: https://github.com/tower-rs/tower
[console]: https://github.com/tokio-rs/console
[tracing]: https://github.com/tokio-rs/tracing
