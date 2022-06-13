use core::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use embassy::{
    blocking_mutex::{raw::RawMutex, Mutex},
    waitqueue::WakerRegistration,
};
use heapless::Deque;

/// A broadcast channel implementation where multiple publishers can send messages to multiple subscribers
///
/// Any published message can be read by all subscribers.
/// A publisher can choose how it sends its message.
///
/// - With [Publisher::publish] the publisher has to wait until there is space in the internal message queue.
/// - With [Publisher::publish_immediate] the publisher doesn't await and instead lets the oldest message
/// in the queue drop if necessary. This will cause any [Subscriber] that missed the message to receive
/// an error to indicate that it has lagged.
pub struct PubSubChannel<M: RawMutex, T: Clone, const N: usize, const SW: usize, const PW: usize> {
    inner: Mutex<M, RefCell<PubSubState<T, N, SW, PW>>>,
}

impl<M: RawMutex, T: Clone, const N: usize, const SW: usize, const PW: usize>
    PubSubChannel<M, T, N, SW, PW>
{
    /// Create a new channel
    pub const fn new() -> Self {
        Self {
            inner: Mutex::const_new(M::INIT, RefCell::new(PubSubState::new())),
        }
    }

    /// Create a new subscriber. It will only receive messages that are published after its creation.
    ///
    /// If there are no subscriber slots left, an error will be returned.
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

    /// Create a new publisher
    ///
    /// If there are no publisher slots left, an error will be returned.
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

    /// Create a new publisher that can only send immediate messages.
    /// This kind of publisher does not take up a publisher slot.
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

            // Make space in the queue if required
            if s.queue.is_full() {
                s.queue.pop_front();
            }

            // This will succeed because we made sure there is space
            self.try_publish(message);
        });
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

/// Internal state for the PubSub channel
struct PubSubState<T: Clone, const N: usize, const SW: usize, const PW: usize> {
    /// The queue contains the last messages that have been published and a countdown of how many subscribers are yet to read it
    queue: Deque<(T, usize), N>,
    /// Every message has an id.
    /// Don't worry, we won't run out.
    /// If a million messages were published every second, then the ID's would run out in about 584942 years.
    next_message_id: u64,
    /// Collection of wakers for Subscribers that are waiting.  
    /// The [Subscriber::subscriber_index] field indexes into this array.
    subscriber_wakers: [Option<WakerRegistration>; SW],
    /// Collection of wakers for Publishers that are waiting.  
    /// The [Publisher::publisher_index] field indexes into this array.
    publisher_wakers: [Option<WakerRegistration>; PW],
}

impl<T: Clone, const N: usize, const SW: usize, const PW: usize> PubSubState<T, N, SW, PW> {
    /// Create a new internal channel state
    const fn new() -> Self {
        const WAKER_INIT: Option<WakerRegistration> = None;
        Self {
            queue: Deque::new(),
            next_message_id: 0,
            subscriber_wakers: [WAKER_INIT; SW],
            publisher_wakers: [WAKER_INIT; PW],
        }
    }
}

/// A subscriber to a channel
/// 
/// This instance carries a reference to the channel, but uses a trait object for it so that the channel's
/// generics are erased on this subscriber
pub struct Subscriber<'a, T: Clone> {
    /// Our index into the channel
    subscriber_index: usize,
    /// The message id of the next message we are yet to receive
    next_message_id: u64,
    /// The channel we are a subscriber to
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

/// A publisher to a channel
/// 
/// This instance carries a reference to the channel, but uses a trait object for it so that the channel's
/// generics are erased on this subscriber

pub struct Publisher<'a, T: Clone> {
    /// Our index into the channel
    publisher_index: usize,
    /// The channel we are a publisher for
    channel: &'a dyn PubSubBehavior<T>,
}

impl<'a, T: Clone> Publisher<'a, T> {
    /// Publish a message right now even when the queue is full.
    /// This may cause a subscriber to miss an older message.
    pub fn publish_immediate(&mut self, message: T) {
        self.channel.publish_immediate(message)
    }

    /// Publish a message. But if the message queue is full, wait for all subscribers to have read the last message
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
    /// The channel we are a publisher for
    channel: &'a dyn PubSubBehavior<T>,
}

impl<'a, T: Clone> ImmediatePublisher<'a, T> {
    /// Publish the message right now even when the queue is full.
    /// This may cause a subscriber to miss an older message.
    pub fn publish_immediate(&mut self, message: T) {
        self.channel.publish_immediate(message)
    }
}

/// Error type for the [PubSubChannel]
#[derive(Debug)]
pub enum Error {
    /// All subscriber slots are used. To add another subscriber, first another subscriber must be dropped or
    /// the capacity of the channels must be increased.
    MaximumSubscribersReached,
    /// All publisher slots are used. To add another publisher, first another publisher must be dropped or
    /// the capacity of the channels must be increased.
    MaximumPublishersReached,
}

trait PubSubBehavior<T> {
    /// Try to publish a message. If the queue is full it won't succeed
    fn try_publish(&self, message: T) -> TryPublishResult;
    /// Publish a message immediately. If the queue is full, just throw out the oldest one.
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

/// Future for the subscriber wait action
pub struct SubscriberWaitFuture<'s, 'a, T: Clone> {
    subscriber: &'s mut Subscriber<'a, T>,
}

impl<'s, 'a, T: Clone> Future for SubscriberWaitFuture<'s, 'a, T> {
    type Output = WaitResult<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if we can read a message
        match self
            .subscriber
            .channel
            .get_message(self.subscriber.next_message_id)
        {
            // Yes, so we are done polling
            Some(WaitResult::Message(message)) => {
                self.subscriber.next_message_id += 1;
                Poll::Ready(WaitResult::Message(message))
            }
            // No, so we need to reregister our waker and sleep again
            None => {
                unsafe {
                    self.subscriber
                        .channel
                        .register_subscriber_waker(self.subscriber.subscriber_index, cx.waker());
                }
                Poll::Pending
            }
            // We missed a couple of messages. We must do our internal bookkeeping and return that we lagged
            Some(WaitResult::Lagged(amount)) => {
                self.subscriber.next_message_id += amount;
                Poll::Ready(WaitResult::Lagged(amount))
            }
        }
    }
}

/// Future for the publisher wait action
pub struct PublisherWaitFuture<'s, 'a, T: Clone> {
    /// The message we need to publish
    message: T,
    publisher: &'s mut Publisher<'a, T>,
}

impl<'s, 'a, T: Clone> Future for PublisherWaitFuture<'s, 'a, T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Try to publish the message
        match self.publisher.channel.try_publish(self.message.clone()) {
            // We did it, we are ready
            TryPublishResult::Done => Poll::Ready(()),
            // The queue is full, so we need to reregister our waker and go to sleep
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

/// The result of the subscriber wait procedure
#[derive(Debug)]
pub enum WaitResult<T> {
    /// The subscriber did not receive all messages and lagged by the given amount of messages. 
    /// (This is the amount of messages that were missed)
    Lagged(u64),
    /// A message was received
    Message(T),
}

enum TryPublishResult {
    Done,
    QueueFull,
}
