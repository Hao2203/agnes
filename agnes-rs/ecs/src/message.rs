use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Marker trait for values carried through the typed message bus.
pub trait Message: Any + Send + Sync {}

impl<T: Any + Send + Sync> Message for T {}

/// An opaque reader identity used to track an independent cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReaderId(u64);

pub(crate) trait ErasedMessageQueue: Any + Send {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn flush_incoming(&mut self);
    fn flush(&mut self);
    fn has_unread(&self) -> bool;
}

struct QueueInner<T: Message> {
    published: VecDeque<(u64, Arc<T>)>,
    pending: Vec<T>,
    incoming: Vec<T>,
    next_seq: u64,
    next_reader: u64,
    readers: HashMap<ReaderId, u64>,
}

pub(crate) struct MessageQueue<T: Message> {
    inner: RefCell<QueueInner<T>>,
}

impl<T: Message> MessageQueue<T> {
    pub(crate) fn new() -> Self {
        Self {
            inner: RefCell::new(QueueInner {
                published: VecDeque::new(),
                pending: Vec::new(),
                incoming: Vec::new(),
                next_seq: 0,
                next_reader: 0,
                readers: HashMap::new(),
            }),
        }
    }

    pub(crate) fn register_reader(&self) -> ReaderId {
        let mut inner = self.inner.borrow_mut();
        let reader = ReaderId(inner.next_reader);
        inner.next_reader = inner
            .next_reader
            .checked_add(1)
            .expect("message reader id space exhausted");
        let next_seq = inner.next_seq;
        inner.readers.insert(reader, next_seq);
        reader
    }

    /// Queues a message written from a system.
    pub(crate) fn write(&self, message: T) {
        self.inner.borrow_mut().pending.push(message);
    }

    /// Queues a host message for publication at the start of the next run.
    pub(crate) fn queue(&self, message: T) {
        self.inner.borrow_mut().incoming.push(message);
    }

    /// Publishes host messages so all systems in the upcoming run can read them.
    pub(crate) fn flush_incoming(&self) {
        let inner = &mut *self.inner.borrow_mut();
        let incoming = std::mem::take(&mut inner.incoming);
        for message in incoming {
            let seq = inner.next_seq;
            inner.next_seq = inner
                .next_seq
                .checked_add(1)
                .expect("message sequence space exhausted");
            inner.published.push_back((seq, Arc::new(message)));
        }
    }

    pub(crate) fn read(&self, reader: ReaderId) -> Vec<Arc<T>> {
        let inner = &mut *self.inner.borrow_mut();
        let Some(cursor) = inner.readers.get_mut(&reader) else {
            return Vec::new();
        };
        // Sequences grow monotonically, so binary search to the reader's
        // position instead of scanning every published message.
        let start = inner.published.partition_point(|(seq, _)| *seq < *cursor);
        let mut messages = Vec::with_capacity(inner.published.len() - start);
        messages.extend(
            inner
                .published
                .iter()
                .skip(start)
                .map(|(_, message)| Arc::clone(message)),
        );
        if let Some((last, _)) = inner.published.back() {
            let next = last
                .checked_add(1)
                .expect("message sequence space exhausted");
            *cursor = (*cursor).max(next);
        }
        messages
    }

    fn flush(&self) {
        let inner = &mut *self.inner.borrow_mut();
        let pending = std::mem::take(&mut inner.pending);
        for message in pending {
            let seq = inner.next_seq;
            inner.next_seq = inner
                .next_seq
                .checked_add(1)
                .expect("message sequence space exhausted");
            inner.published.push_back((seq, Arc::new(message)));
        }

        let retain_from = inner.readers.values().copied().min();
        match retain_from {
            Some(minimum) => {
                while inner
                    .published
                    .front()
                    .is_some_and(|(seq, _)| *seq < minimum)
                {
                    inner.published.pop_front();
                }
            }
            None => inner.published.clear(),
        }
    }

    #[cfg(test)]
    pub(crate) fn published_len(&self) -> usize {
        self.inner.borrow().published.len()
    }

    pub(crate) fn has_unread(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.pending.is_empty()
            || !inner.incoming.is_empty()
            || inner
                .readers
                .values()
                .any(|cursor| *cursor < inner.next_seq)
    }
}

impl<T: Message> ErasedMessageQueue for MessageQueue<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn flush_incoming(&mut self) {
        MessageQueue::flush_incoming(self);
    }

    fn flush(&mut self) {
        MessageQueue::flush(self);
    }

    fn has_unread(&self) -> bool {
        MessageQueue::has_unread(self)
    }
}

/// Reads messages of type `T` from its own cursor.
///
/// Messages written by [`MessageWriter`] during a schedule run become visible
/// at the start of the next run. Messages published by the host through
/// [`World::send_message`](crate::World::send_message) are visible in the same
/// run.
pub struct MessageReader<'w, T: Message> {
    queue: &'w MessageQueue<T>,
    reader: ReaderId,
}

impl<'w, T: Message> MessageReader<'w, T> {
    pub(crate) fn new(queue: &'w MessageQueue<T>, reader: ReaderId) -> Self {
        Self { queue, reader }
    }

    /// Reads all messages after this reader's cursor and advances it.
    pub fn read(&mut self) -> Vec<Arc<T>> {
        self.queue.read(self.reader)
    }
}

/// Writes messages of type `T`.
///
/// Writes are buffered and published when the schedule finishes.
pub struct MessageWriter<'w, T: Message> {
    queue: &'w MessageQueue<T>,
}

impl<'w, T: Message> MessageWriter<'w, T> {
    pub(crate) fn new(queue: &'w MessageQueue<T>) -> Self {
        Self { queue }
    }

    /// Queues one message for publication at the end of the schedule run.
    pub fn write(&mut self, message: T) {
        self.queue.write(message);
    }
}
