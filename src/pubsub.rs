use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use embassy::blocking_mutex::raw::RawMutex;
use embassy::blocking_mutex::Mutex;
use embassy::waitqueue::WakerRegistration;
use heapless::Deque;

pub struct PubSubChannel<M: RawMutex, T: Clone, const N: usize, const SW: usize, const PW: usize> {
    inner: Mutex<M, RefCell<PubSubState<T, N, SW, PW>>>,
}

impl<M: RawMutex, T: Clone, const N: usize, const SW: usize, const PW: usize>
    PubSubChannel<M, T, N, SW, PW>
{
    pub const fn new() -> Self {
        Self {
            inner: Mutex::const_new(M::INIT, RefCell::new(PubSubState::new())),
        }
    }

    pub fn subscriber(&self) -> Result<Subscriber<'_, T>, Error> {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            // Search for an empty subscriber spot
            for (i, sub_spot) in s.subscriber_wakers.iter_mut().enumerate() {
                if sub_spot.is_none() {
                    // We've found a spot, so now fill it and create the subscriber
                    *sub_spot = Some(WakerRegistration::new());
                    return Ok(Subscriber {
                        subscriber_index: i,
                        next_message_id: s.next_message_id,
                        channel: self,
                    });
                }
            }

            // No spot was found, we're full
            Err(Error::MaximumSubscribersReached)
        })
    }

    pub fn publisher(&self) -> Result<Publisher<'_, T>, Error> {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            // Search for an empty publisher spot
            for (i, pub_spot) in s.publisher_wakers.iter_mut().enumerate() {
                if pub_spot.is_none() {
                    // We've found a spot, so now fill it and create the subscriber
                    *pub_spot = Some(WakerRegistration::new());
                    return Ok(Publisher {
                        publisher_index: i,
                        channel: self,
                    });
                }
            }

            // No spot was found, we're full
            Err(Error::MaximumPublishersReached)
        })
    }

    pub fn immediate_publisher(&self) -> ImmediatePublisher<'_, T> {
        ImmediatePublisher { channel: self }
    }
}

impl<M: RawMutex, T: Clone, const N: usize, const SW: usize, const PW: usize> PubSubBehavior<T>
    for PubSubChannel<M, T, N, SW, PW>
{
    fn try_publish(&self, message: T) -> TryPublishResult {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            let active_subscriber_count = s.subscriber_wakers.iter().flatten().count();

            if s.queue.is_full() {
                return TryPublishResult::QueueFull;
            }
            // We just did a check for this
            unsafe {
                s.queue
                    .push_back_unchecked((message.clone(), active_subscriber_count));
            }

            s.next_message_id += 1;

            // Wake all of the subscribers
            for active_subscriber in s.subscriber_wakers.iter_mut().flatten() {
                active_subscriber.wake()
            }

            TryPublishResult::Done
        })
    }

    fn publish_immediate(&self, message: T) {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            if s.queue.is_full() {
                s.queue.pop_front();
            }
        });

        self.try_publish(message);
    }

    fn get_message(&self, message_id: u64) -> Option<WaitResult<T>> {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            let start_id = s.next_message_id - s.queue.len() as u64;

            if message_id < start_id {
                return Some(WaitResult::Lagged(start_id - message_id));
            }

            let current_message_index = (message_id - start_id) as usize;

            if current_message_index >= s.queue.len() {
                return None;
            }

            // We've checked that the index is valid
            unsafe {
                let queue_item = s
                    .queue
                    .iter_mut()
                    .nth(current_message_index)
                    .unwrap_unchecked();

                // We're reading this item, so decrement the counter
                queue_item.1 -= 1;
                let message = queue_item.0.clone();

                if current_message_index == 0 && queue_item.1 == 0 {
                    s.queue.pop_front();
                    s.publisher_wakers
                        .iter_mut()
                        .flatten()
                        .for_each(|w| w.wake());
                }

                Some(WaitResult::Message(message))
            }
        })
    }

    unsafe fn register_subscriber_waker(&self, subscriber_index: usize, waker: &Waker) {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();
            s.subscriber_wakers
                .get_unchecked_mut(subscriber_index)
                .as_mut()
                .unwrap_unchecked()
                .register(waker);
        })
    }

    unsafe fn register_publisher_waker(&self, publisher_index: usize, waker: &Waker) {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();
            s.publisher_wakers
                .get_unchecked_mut(publisher_index)
                .as_mut()
                .unwrap_unchecked()
                .register(waker);
        })
    }

    unsafe fn unregister_subscriber(
        &self,
        subscriber_index: usize,
        subscriber_next_message_id: u64,
    ) {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();

            // Remove the subscriber from the wakers
            *s.subscriber_wakers.get_unchecked_mut(subscriber_index) = None;

            // All messages that haven't been read yet by this subscriber must have their counter decremented
            let start_id = s.next_message_id - s.queue.len() as u64;
            if subscriber_next_message_id >= start_id {
                let current_message_index = (subscriber_next_message_id - start_id) as usize;
                s.queue
                    .iter_mut()
                    .skip(current_message_index)
                    .for_each(|(_, counter)| *counter -= 1);
            }
        })
    }

    unsafe fn unregister_publisher(&self, publisher_index: usize) {
        self.inner.lock(|inner| {
            let mut s = inner.borrow_mut();
            // Remove the publisher from the wakers
            *s.publisher_wakers.get_unchecked_mut(publisher_index) = None;
        })
    }
}

pub struct PubSubState<T: Clone, const N: usize, const SW: usize, const PW: usize> {
    /// The queue contains the last messages that have been published and a countdown of how many subscribers are yet to read it
    queue: Deque<(T, usize), N>,
    next_message_id: u64,
    subscriber_wakers: [Option<WakerRegistration>; SW],
    publisher_wakers: [Option<WakerRegistration>; PW],
}

impl<T: Clone, const N: usize, const SW: usize, const PW: usize> PubSubState<T, N, SW, PW> {
    pub const fn new() -> Self {
        const WAKER_INIT: Option<WakerRegistration> = None;
        Self {
            queue: Deque::new(),
            next_message_id: 0,
            subscriber_wakers: [WAKER_INIT; SW],
            publisher_wakers: [WAKER_INIT; PW],
        }
    }
}

pub struct Subscriber<'a, T: Clone> {
    subscriber_index: usize,
    next_message_id: u64,
    channel: &'a dyn PubSubBehavior<T>,
}

impl<'a, T: Clone> Subscriber<'a, T> {
    pub fn wait<'s>(&'s mut self) -> SubscriberWaitFuture<'s, 'a, T> {
        SubscriberWaitFuture { subscriber: self }
    }
}

impl<'a, T: Clone> Drop for Subscriber<'a, T> {
    fn drop(&mut self) {
        unsafe {
            self.channel
                .unregister_subscriber(self.subscriber_index, self.next_message_id)
        }
    }
}

pub struct Publisher<'a, T: Clone> {
    publisher_index: usize,
    channel: &'a dyn PubSubBehavior<T>,
}

impl<'a, T: Clone> Publisher<'a, T> {
    /// Publish the message right now even when the queue is full.
    /// This may cause a subscriber to miss an older message.
    pub fn publish_immediate(&mut self, message: T) {
        self.channel.publish_immediate(message)
    }

    pub fn publish<'s>(&'s mut self, message: T) -> PublisherWaitFuture<'s, 'a, T> {
        PublisherWaitFuture {
            message,
            publisher: self,
        }
    }
}

impl<'a, T: Clone> Drop for Publisher<'a, T> {
    fn drop(&mut self) {
        unsafe { self.channel.unregister_publisher(self.publisher_index) }
    }
}

/// A publisher that can only use the `publish_immediate` function, but it doesn't have to be registered with the channel.
/// (So an infinite amount is possible)
pub struct ImmediatePublisher<'a, T: Clone> {
    channel: &'a dyn PubSubBehavior<T>,
}

impl<'a, T: Clone> ImmediatePublisher<'a, T> {
    /// Publish the message right now even when the queue is full.
    /// This may cause a subscriber to miss an older message.
    pub fn publish_immediate(&mut self, message: T) {
        self.channel.publish_immediate(message)
    }
}

#[derive(Debug)]
pub enum Error {
    MaximumSubscribersReached,
    MaximumPublishersReached,
}

trait PubSubBehavior<T> {
    fn try_publish(&self, message: T) -> TryPublishResult;
    fn publish_immediate(&self, message: T);
    /// Tries to read the message if available
    fn get_message(&self, message_id: u64) -> Option<WaitResult<T>>;
    /// Register the given waker for the given subscriber.
    ///
    /// ## Safety
    ///
    /// The subscriber index must be of a valid and active subscriber
    unsafe fn register_subscriber_waker(&self, subscriber_index: usize, waker: &Waker);
    /// Register the given waker for the given publisher.
    ///
    /// ## Safety
    ///
    /// The subscriber index must be of a valid and active publisher
    unsafe fn register_publisher_waker(&self, publisher_index: usize, waker: &Waker);
    /// Make the channel forget the subscriber.
    ///
    /// ## Safety
    ///
    /// The subscriber index must be of a valid and active subscriber which must not be used again
    /// unless a new subscriber takes on that index.
    unsafe fn unregister_subscriber(
        &self,
        subscriber_index: usize,
        subscriber_next_message_id: u64,
    );
    /// Make the channel forget the publisher.
    ///
    /// ## Safety
    ///
    /// The publisher index must be of a valid and active publisher which must not be used again
    /// unless a new publisher takes on that index.
    unsafe fn unregister_publisher(&self, publisher_index: usize);
}

pub struct SubscriberWaitFuture<'s, 'a, T: Clone> {
    subscriber: &'s mut Subscriber<'a, T>,
}

impl<'s, 'a, T: Clone> Future for SubscriberWaitFuture<'s, 'a, T> {
    type Output = WaitResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self
            .subscriber
            .channel
            .get_message(self.subscriber.next_message_id)
        {
            Some(WaitResult::Message(message)) => {
                self.subscriber.next_message_id += 1;
                Poll::Ready(WaitResult::Message(message))
            }
            None => {
                unsafe {
                    self.subscriber
                        .channel
                        .register_subscriber_waker(self.subscriber.subscriber_index, cx.waker());
                }
                Poll::Pending
            }
            Some(WaitResult::Lagged(amount)) => {
                self.subscriber.next_message_id += amount;
                Poll::Ready(WaitResult::Lagged(amount))
            }
        }
    }
}

pub struct PublisherWaitFuture<'s, 'a, T: Clone> {
    message: T,
    publisher: &'s mut Publisher<'a, T>,
}

impl<'s, 'a, T: Clone> Future for PublisherWaitFuture<'s, 'a, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.publisher.channel.try_publish(self.message.clone()) {
            TryPublishResult::Done => Poll::Ready(()),
            TryPublishResult::QueueFull => {
                unsafe {
                    self.publisher
                        .channel
                        .register_publisher_waker(self.publisher.publisher_index, cx.waker());
                }
                Poll::Pending
            }
        }
    }
}

#[derive(Debug)]
pub enum WaitResult<T> {
    Lagged(u64),
    Message(T),
}

enum TryPublishResult {
    Done,
    QueueFull,
}
