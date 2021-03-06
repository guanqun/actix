use std::cell::Cell;
use futures::unsync::oneshot::{channel, Receiver};
use futures::sync::oneshot::{channel as sync_channel, Receiver as SyncReceiver};

use actor::{Actor, Handler, ResponseType, AsyncContext};
use context::{ContextProtocol, AsyncContextApi};
use envelope::{Envelope, ToEnvelope};
use message::Request;
use queue::{sync, unsync};


/// Trait give access to actor's address
pub trait ActorAddress<A, T> where A: Actor {
    /// Returns actor's address for specific context
    fn get(ctx: &mut A::Context) -> T;
}

impl<A> ActorAddress<A, Address<A>> for A
    where A: Actor,
          A::Context: AsyncContext<A> + AsyncContextApi<A>
{
    fn get(ctx: &mut A::Context) -> Address<A> {
        ctx.address_cell().unsync_address()
    }
}

impl<A> ActorAddress<A, SyncAddress<A>> for A
    where A: Actor,
          A::Context: AsyncContext<A> + AsyncContextApi<A>
{
    fn get(ctx: &mut A::Context) -> SyncAddress<A> {
        ctx.address_cell().sync_address()
    }
}

impl<A> ActorAddress<A, (Address<A>, SyncAddress<A>)> for A
    where A: Actor,
          A::Context: AsyncContext<A> + AsyncContextApi<A>
{
    fn get(ctx: &mut A::Context) -> (Address<A>, SyncAddress<A>)
    {
        (ctx.address_cell().unsync_address(), ctx.address_cell().sync_address())
    }
}

impl<A> ActorAddress<A, ()> for A where A: Actor {
    fn get(_: &mut A::Context) -> () {
        ()
    }
}

pub trait Subscriber<M: 'static> {
    /// Send buffered message
    fn send(&self, msg: M) -> Result<(), M>;

    /// Create boxed clone of the current subscriber
    fn boxed(&self) -> Box<Subscriber<M>>;
}

/// Convenience impl to allow boxed Subscriber objects to be cloned using `Clone.clone()`.
impl<M: 'static> Clone for Box<Subscriber<M>> {
    fn clone(&self) -> Box<Subscriber<M>> {
        self.boxed()
    }
}

/// Address of the actor
///
/// Actor has to run in the same thread as owner of the address.
pub struct Address<A> where A: Actor, A::Context: AsyncContext<A> {
    tx: unsync::UnboundedSender<ContextProtocol<A>>
}

impl<A> Clone for Address<A> where A: Actor, A::Context: AsyncContext<A> {
    fn clone(&self) -> Self {
        Address{tx: self.tx.clone() }
    }
}

impl<A> Address<A> where A: Actor, A::Context: AsyncContext<A> {

    pub(crate) fn new(sender: unsync::UnboundedSender<ContextProtocol<A>>) -> Address<A> {
        Address{tx: sender}
    }

    /// Indicates if address is still connected to the actor.
    pub fn connected(&self) -> bool {
        self.tx.connected()
    }

    /// Send message `M` to actor `A`. Communication channel to the actor is
    /// unbuonded.
    pub fn send<M>(&self, msg: M)
        where A: Handler<M>,
              M: ResponseType + 'static
    {
        let _ = self.tx.unbounded_send(
            ContextProtocol::Envelope(Envelope::local(msg, None)));
    }

    /// Send message to actor `A` and asyncronously wait for response.
    /// Communication channel to the actor is unbuonded.
    pub fn call<B, M>(&self, _: &B, msg: M) -> Request<B, M>
        where A: Handler<M>,
              B: Actor,
              M: ResponseType + 'static
    {
        let (tx, rx) = channel();
        let _ = self.tx.unbounded_send(
            ContextProtocol::Envelope(Envelope::local(msg, Some(tx))));

        Request::local(rx)
    }

    /// Send message to the actor `A` and asyncronously wait for response.
    pub fn call_fut<M>(&self, msg: M) -> Receiver<Result<M::Item, M::Error>>
        where A: Handler<M>,
              M: ResponseType + 'static
    {
        let (tx, rx) = channel();
        let _ = self.tx.unbounded_send(
            ContextProtocol::Envelope(Envelope::local(msg, Some(tx))));

        rx
    }

    /// Upgrade address to SyncAddress.
    pub fn upgrade(&self) -> Receiver<SyncAddress<A>> {
        let (tx, rx) = channel();
        let _ = self.tx.unbounded_send(
            ContextProtocol::Upgrade(tx));

        rx
    }

    /// Get `Subscriber` for specific message type
    pub fn subscriber<M>(&self) -> Box<Subscriber<M>>
        where A: Handler<M>,
              M: ResponseType + 'static
    {
        Box::new(Clone::clone(self))
    }
}

impl<A, M> Subscriber<M> for Address<A>
    where A: Actor + Handler<M>,
          A::Context: AsyncContext<A>,
          M: ResponseType + 'static
{
    fn send(&self, msg: M) -> Result<(), M> {
        if self.connected() {
            self.send(msg);
            Ok(())
        } else {
            Err(msg)
        }
    }

    fn boxed(&self) -> Box<Subscriber<M>> {
        Box::new(self.clone())
    }
}

/// `Send` address of the actor. Actor can run in differend thread
pub struct SyncAddress<A> where A: Actor {
    tx: sync::UnboundedSender<Envelope<A>>,
    closed: Cell<bool>,
}

unsafe impl<A> Send for SyncAddress<A> where A: Actor {}
unsafe impl<A> Sync for SyncAddress<A> where A: Actor {}

impl<A> Clone for SyncAddress<A> where A: Actor {
    fn clone(&self) -> Self {
        SyncAddress{tx: self.tx.clone(), closed: self.closed.clone()}
    }
}

impl<A> SyncAddress<A> where A: Actor {

    pub(crate) fn new(sender: sync::UnboundedSender<Envelope<A>>) -> SyncAddress<A> {
        SyncAddress{tx: sender, closed: Cell::new(false)}
    }

    /// Indicates if address is still connected to the actor.
    pub fn connected(&self) -> bool {
        !self.closed.get()
    }

    /// Send message `M` to actor `A`. Message cold be sent to actor running in
    /// different thread.
    pub fn send<M>(&self, msg: M)
        where A: Handler<M>, <A as Actor>::Context: ToEnvelope<A>,
              M: ResponseType + Send + 'static,
              M::Item: Send,
              M::Error: Send,
    {
        if self.tx.unbounded_send(
            <<A as Actor>::Context as ToEnvelope<A>>::pack(msg, None)).is_err()
        {
            self.closed.set(true)
        }
    }

    /// Send message to actor `A` and asyncronously wait for response.
    pub fn call<B: Actor, M>(&self, _: &B, msg: M) -> Request<B, M>
        where A: Handler<M>,
              M: ResponseType + Send + 'static,
              M::Item: Send,
              M::Error: Send,
             <A as Actor>::Context: ToEnvelope<A>,
    {
        let (tx, rx) = sync_channel();
        if self.tx.unbounded_send(
            <<A as Actor>::Context as ToEnvelope<A>>::pack(msg, Some(tx))).is_err()
        {
            self.closed.set(true)
        }

        Request::remote(rx)
    }

    /// Send message to actor `A` and asyncronously wait for response.
    pub fn call_fut<M>(&self, msg: M) -> SyncReceiver<Result<M::Item, M::Error>>
        where A: Handler<M>,
              M: ResponseType + Send + 'static,
              M::Item: Send,
              M::Error: Send,
             <A as Actor>::Context: ToEnvelope<A>,
    {
        let (tx, rx) = sync_channel();
        if self.tx.unbounded_send(
            <<A as Actor>::Context as ToEnvelope<A>>::pack(msg, Some(tx))).is_err()
        {
            self.closed.set(true)
        }

        rx
    }

    /// Get `Subscriber` for specific message type
    pub fn subscriber<M: 'static + Send>(&self) -> Box<Subscriber<M> + Send>
        where A: Handler<M>,
              M: ResponseType + Send + 'static,
              M::Item: Send,
              M::Error: Send,
             <A as Actor>::Context: ToEnvelope<A>,
    {
        Box::new(self.clone())
    }
}

impl<A, M> Subscriber<M> for SyncAddress<A>
    where A: Actor + Handler<M>,
          <A as Actor>::Context: ToEnvelope<A>,
          M: ResponseType + Send + 'static,
          M::Item: Send,
          M::Error: Send,
{
    fn send(&self, msg: M) -> Result<(), M> {
        if self.connected() {
            self.send(msg);
            Ok(())
        } else {
            Err(msg)
        }
    }

    fn boxed(&self) -> Box<Subscriber<M>> {
        Box::new(self.clone())
    }
}
