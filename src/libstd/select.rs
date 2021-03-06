// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use cell::Cell;
use comm;
use container::Container;
use iter::{Iterator, DoubleEndedIterator};
use option::*;
// use either::{Either, Left, Right};
// use rt::kill::BlockedTask;
use rt::sched::Scheduler;
use rt::select::{SelectInner, SelectPortInner};
use rt::local::Local;
use rt::rtio::EventLoop;
use task;
use unstable::finally::Finally;
use vec::{OwnedVector, MutableVector};

/// Trait for message-passing primitives that can be select()ed on.
pub trait Select : SelectInner { }

/// Trait for message-passing primitives that can use the select2() convenience wrapper.
// (This is separate from the above trait to enable heterogeneous lists of ports
// that implement Select on different types to use select().)
pub trait SelectPort<T> : SelectPortInner<T> { }

/// Receive a message from any one of many ports at once. Returns the index of the
/// port whose data is ready. (If multiple are ready, returns the lowest index.)
pub fn select<A: Select>(ports: &mut [A]) -> uint {
    if ports.is_empty() {
        fail2!("can't select on an empty list");
    }

    for (index, port) in ports.mut_iter().enumerate() {
        if port.optimistic_check() {
            return index;
        }
    }

    // If one of the ports already contains data when we go to block on it, we
    // don't bother enqueueing on the rest of them, so we shouldn't bother
    // unblocking from it either. This is just for efficiency, not correctness.
    // (If not, we need to unblock from all of them. Length is a placeholder.)
    let mut ready_index = ports.len();

    // XXX: We're using deschedule...and_then in an unsafe way here (see #8132),
    // in that we need to continue mutating the ready_index in the environment
    // after letting the task get woken up. The and_then closure needs to delay
    // the task from resuming until all ports have become blocked_on.
    let (p,c) = comm::oneshot();
    let p = Cell::new(p);
    let c = Cell::new(c);

    do (|| {
        let c = Cell::new(c.take());
        let sched: ~Scheduler = Local::take();
        do sched.deschedule_running_task_and_then |sched, task| {
            let task_handles = task.make_selectable(ports.len());

            for (index, (port, task_handle)) in
                    ports.mut_iter().zip(task_handles.move_iter()).enumerate() {
                // If one of the ports has data by now, it will wake the handle.
                if port.block_on(sched, task_handle) {
                    ready_index = index;
                    break;
                }
            }

            let c = Cell::new(c.take());
            do sched.event_loop.callback { c.take().send_deferred(()) }
        }
    }).finally {
        let p = Cell::new(p.take());
        // Unkillable is necessary not because getting killed is dangerous here,
        // but to force the recv not to use the same kill-flag that we used for
        // selecting. Otherwise a user-sender could spuriously wakeup us here.
        do task::unkillable { p.take().recv(); }
    }

    // Task resumes. Now unblock ourselves from all the ports we blocked on.
    // If the success index wasn't reset, 'take' will just take all of them.
    // Iterate in reverse so the 'earliest' index that's ready gets returned.
    for (index, port) in ports.mut_slice(0, ready_index).mut_iter().enumerate().invert() {
        if port.unblock_from() {
            ready_index = index;
        }
    }

    assert!(ready_index < ports.len());
    return ready_index;
}

/* FIXME(#5121, #7914) This all should be legal, but rust is not clever enough yet.

impl <'self> Select for &'self mut Select {
    fn optimistic_check(&mut self) -> bool { self.optimistic_check() }
    fn block_on(&mut self, sched: &mut Scheduler, task: BlockedTask) -> bool {
        self.block_on(sched, task)
    }
    fn unblock_from(&mut self) -> bool { self.unblock_from() }
}

pub fn select2<TA, A: SelectPort<TA>, TB, B: SelectPort<TB>>(mut a: A, mut b: B)
        -> Either<(Option<TA>, B), (A, Option<TB>)> {
    let result = {
        let mut ports = [&mut a as &mut Select, &mut b as &mut Select];
        select(ports)
    };
    match result {
        0 => Left ((a.recv_ready(), b)),
        1 => Right((a, b.recv_ready())),
        x => fail2!("impossible case in select2: {:?}", x)
    }
}

*/

#[cfg(test)]
mod test {
    use super::*;
    use clone::Clone;
    use num::Times;
    use option::*;
    use rt::comm::*;
    use rt::test::*;
    use vec::*;
    use comm::GenericChan;
    use task;
    use cell::Cell;
    use iter::{Iterator, range};

    #[test] #[should_fail]
    fn select_doesnt_get_trolled() {
        select::<PortOne<()>>([]);
    }

    /* non-blocking select tests */

    #[cfg(test)]
    fn select_helper(num_ports: uint, send_on_chans: &[uint]) {
        // Unfortunately this does not actually test the block_on early-break
        // codepath in select -- racing between the sender and the receiver in
        // separate tasks is necessary to get around the optimistic check.
        let (ports, chans) = unzip(range(0, num_ports).map(|_| oneshot::<()>()));
        let mut dead_chans = ~[];
        let mut ports = ports;
        for (i, chan) in chans.move_iter().enumerate() {
            if send_on_chans.contains(&i) {
                chan.send(());
            } else {
                dead_chans.push(chan);
            }
        }
        let ready_index = select(ports);
        assert!(send_on_chans.contains(&ready_index));
        assert!(ports.swap_remove(ready_index).recv_ready().is_some());
        let _ = dead_chans;

        // Same thing with streams instead.
        // FIXME(#7971): This should be in a macro but borrowck isn't smart enough.
        let (ports, chans) = unzip(range(0, num_ports).map(|_| stream::<()>()));
        let mut dead_chans = ~[];
        let mut ports = ports;
        for (i, chan) in chans.move_iter().enumerate() {
            if send_on_chans.contains(&i) {
                chan.send(());
            } else {
                dead_chans.push(chan);
            }
        }
        let ready_index = select(ports);
        assert!(send_on_chans.contains(&ready_index));
        assert!(ports.swap_remove(ready_index).recv_ready().is_some());
        let _ = dead_chans;
    }

    #[test]
    fn select_one() {
        do run_in_newsched_task { select_helper(1, [0]) }
    }

    #[test]
    fn select_two() {
        // NB. I would like to have a test that tests the first one that is
        // ready is the one that's returned, but that can't be reliably tested
        // with the randomized behaviour of optimistic_check.
        do run_in_newsched_task { select_helper(2, [1]) }
        do run_in_newsched_task { select_helper(2, [0]) }
        do run_in_newsched_task { select_helper(2, [1,0]) }
    }

    #[test]
    fn select_a_lot() {
        do run_in_newsched_task { select_helper(12, [7,8,9]) }
    }

    #[test]
    fn select_stream() {
        use util;
        use comm::GenericChan;

        // Sends 10 buffered packets, and uses select to retrieve them all.
        // Puts the port in a different spot in the vector each time.
        do run_in_newsched_task {
            let (ports, _) = unzip(range(0u, 10).map(|_| stream::<int>()));
            let (port, chan) = stream();
            do 10.times { chan.send(31337); }
            let mut ports = ports;
            let mut port = Some(port);
            let order = [5u,0,4,3,2,6,9,8,7,1];
            for &index in order.iter() {
                // put the port in the vector at any index
                util::swap(port.get_mut_ref(), &mut ports[index]);
                assert!(select(ports) == index);
                // get it back out
                util::swap(port.get_mut_ref(), &mut ports[index]);
                // NB. Not recv(), because optimistic_check randomly fails.
                assert!(port.get_ref().recv_ready().unwrap() == 31337);
            }
        }
    }

    #[test]
    fn select_unkillable() {
        do run_in_newsched_task {
            do task::unkillable { select_helper(2, [1]) }
        }
    }

    /* blocking select tests */

    #[test]
    fn select_blocking() {
        select_blocking_helper(true);
        select_blocking_helper(false);

        fn select_blocking_helper(killable: bool) {
            do run_in_newsched_task {
                let (p1,_c) = oneshot();
                let (p2,c2) = oneshot();
                let mut ports = [p1,p2];

                let (p3,c3) = oneshot();
                let (p4,c4) = oneshot();

                let x = Cell::new((c2, p3, c4));
                do task::spawn {
                    let (c2, p3, c4) = x.take();
                    p3.recv();   // handshake parent
                    c4.send(()); // normal receive
                    task::deschedule();
                    c2.send(()); // select receive
                }

                // Try to block before child sends on c2.
                c3.send(());
                p4.recv();
                if killable {
                    assert!(select(ports) == 1);
                } else {
                    do task::unkillable { assert!(select(ports) == 1); }
                }
            }
        }
    }

    #[test]
    fn select_racing_senders() {
        static NUM_CHANS: uint = 10;

        select_racing_senders_helper(true,  ~[0,1,2,3,4,5,6,7,8,9]);
        select_racing_senders_helper(false, ~[0,1,2,3,4,5,6,7,8,9]);
        select_racing_senders_helper(true,  ~[0,1,2]);
        select_racing_senders_helper(false, ~[0,1,2]);
        select_racing_senders_helper(true,  ~[3,4,5,6]);
        select_racing_senders_helper(false, ~[3,4,5,6]);
        select_racing_senders_helper(true,  ~[7,8,9]);
        select_racing_senders_helper(false, ~[7,8,9]);

        fn select_racing_senders_helper(killable: bool, send_on_chans: ~[uint]) {
            use rt::test::spawntask_random;

            do run_in_newsched_task {
                // A bit of stress, since ordinarily this is just smoke and mirrors.
                do 4.times {
                    let send_on_chans = send_on_chans.clone();
                    do task::spawn {
                        let mut ports = ~[];
                        for i in range(0u, NUM_CHANS) {
                            let (p,c) = oneshot();
                            ports.push(p);
                            if send_on_chans.contains(&i) {
                                let c = Cell::new(c);
                                do spawntask_random {
                                    task::deschedule();
                                    c.take().send(());
                                }
                            }
                        }
                        // nondeterministic result, but should succeed
                        if killable {
                            select(ports);
                        } else {
                            do task::unkillable { select(ports); }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn select_killed() {
        do run_in_newsched_task {
            let (success_p, success_c) = oneshot::<bool>();
            let success_c = Cell::new(success_c);
            do task::try {
                let success_c = Cell::new(success_c.take());
                do task::unkillable {
                    let (p,c) = oneshot();
                    let c = Cell::new(c);
                    do task::spawn {
                        let (dead_ps, dead_cs) = unzip(range(0u, 5).map(|_| oneshot::<()>()));
                        let mut ports = dead_ps;
                        select(ports); // should get killed; nothing should leak
                        c.take().send(()); // must not happen
                        // Make sure dead_cs doesn't get closed until after select.
                        let _ = dead_cs;
                    }
                    do task::spawn {
                        fail2!(); // should kill sibling awake
                    }

                    // wait for killed selector to close (NOT send on) its c.
                    // hope to send 'true'.
                    success_c.take().send(p.try_recv().is_none());
                }
            };
            assert!(success_p.recv());
        }
    }
}
