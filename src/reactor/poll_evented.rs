//! Readiness tracking streams, backing I/O objects.
//!
//! This module contains the core type which is used to back all I/O on object
//! in `tokio-core`. The `PollEvented` type is the implementation detail of
//! all I/O. Each `PollEvented` manages registration with a reactor,
//! acquisition of a token, and tracking of the readiness state on the
//! underlying I/O primitive.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use futures::{task, Async, Poll};
use mio::event::Evented;
use mio::Ready;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio::reactor::{Registration};

use reactor::{Handle, Remote};

/// A concrete implementation of a stream of readiness notifications for I/O
/// objects that originates from an event loop.
///
/// Created by the `PollEvented::new` method, each `PollEvented` is
/// associated with a specific event loop and source of events that will be
/// registered with an event loop.
///
/// An instance of `PollEvented` is essentially the bridge between the `mio`
/// world and the `tokio-core` world, providing abstractions to receive
/// notifications about changes to an object's `mio::Ready` state.
///
/// Each readiness stream has a number of methods to test whether the underlying
/// object is readable or writable. Once the methods return that an object is
/// readable/writable, then it will continue to do so until the `need_read` or
/// `need_write` methods are called.
///
/// That is, this object is typically wrapped in another form of I/O object.
/// It's the responsibility of the wrapper to inform the readiness stream when a
/// "would block" I/O event is seen. The readiness stream will then take care of
/// any scheduling necessary to get notified when the event is ready again.
///
/// You can find more information about creating a custom I/O object [online].
///
/// [online]: https://tokio.rs/docs/going-deeper-tokio/core-low-level/#custom-io
///
/// ## Readiness to read/write
///
/// A `PollEvented` allows listening and waiting for an arbitrary `mio::Ready`
/// instance, including the platform-specific contents of `mio::Ready`. At most
/// two future tasks, however, can be waiting on a `PollEvented`. The
/// `need_read` and `need_write` methods can block two separate tasks, one on
/// reading and one on writing. Not all I/O events correspond to read/write,
/// however!
///
/// To account for this a `PollEvented` gets a little interesting when working
/// with an arbitrary instance of `mio::Ready` that may not map precisely to
/// "write" and "read" tasks. Currently it is defined that instances of
/// `mio::Ready` that do *not* return true from `is_writable` are all notified
/// through `need_read`, or the read task.
///
/// In other words, `poll_ready` with the `mio::UnixReady::hup` event will block
/// the read task of this `PollEvented` if the `hup` event isn't available.
/// Essentially a good rule of thumb is that if you're using the `poll_ready`
/// method you want to also use `need_read` to signal blocking and you should
/// otherwise probably avoid using two tasks on the same `PollEvented`.
pub struct PollEvented<E> {
    io: E,
    inner: Inner,
    remote: Remote,
}

struct Inner {
    registration: Registration,

    /// Currently visible read readiness
    read_readiness: AtomicUsize,

    /// Currently visible write readiness
    write_readiness: AtomicUsize,
}

impl<E: Evented> PollEvented<E> {
    /// Creates a new readiness stream associated with the provided
    /// `loop_handle` and for the given `source`.
    ///
    /// This method returns a future which will resolve to the readiness stream
    /// when it's ready.
    pub fn new(io: E, handle: &Handle) -> io::Result<PollEvented<E>> {
        let registration = Registration::new();
        registration.register_with(&io, handle.new_tokio_handle())?;

        Ok(PollEvented {
            io: io,
            inner: Inner {
                registration,
                read_readiness: AtomicUsize::new(0),
                write_readiness: AtomicUsize::new(0),
            },
            remote: handle.remote().clone(),
        })
    }

    /// Deregisters this source of events from the reactor core specified.
    ///
    /// This method can optionally be called to unregister the underlying I/O
    /// object with the event loop that the `handle` provided points to.
    /// Typically this method is not required as this automatically happens when
    /// `E` is dropped, but for some use cases the `E` object doesn't represent
    /// an owned reference, so dropping it won't automatically unregister with
    /// the event loop.
    ///
    /// This consumes `self` as it will no longer provide events after the
    /// method is called, and will likely return an error if this `PollEvented`
    /// was created on a separate event loop from the `handle` specified.
    pub fn deregister(self, _: &Handle) -> io::Result<()> {
        // Nothing has to happen here anymore as I/O objects are explicitly
        // deregistered before dropped.
        Ok(())
    }
}

impl<E> PollEvented<E> {
    /// Tests to see if this source is ready to be read from or not.
    ///
    /// If this stream is not ready for a read then `NotReady` will be returned
    /// and the current task will be scheduled to receive a notification when
    /// the stream is readable again. In other words, this method is only safe
    /// to call from within the context of a future's task, typically done in a
    /// `Future::poll` method.
    ///
    /// This is mostly equivalent to `self.poll_ready(Ready::readable())`.
    ///
    /// # Panics
    ///
    /// This function will panic if called outside the context of a future's
    /// task.
    pub fn poll_read(&self) -> Async<()> {
        if self.poll_read2().is_ready() {
            return ().into();
        }

        Async::NotReady
    }

    fn poll_read2(&self) -> Async<Ready> {
        // Load the cached readiness
        match self.inner.read_readiness.load(Relaxed) {
            0 => {}
            mut n => {
                // Check what's new with the reactor.
                if let Some(ready) = self.inner.registration.take_read_ready().unwrap() {
                    n |= super::ready2usize(ready);
                    self.inner.read_readiness.store(n, Relaxed);
                }

                return super::usize2ready(n).into();
            }
        }

        let ready = match self.inner.registration.poll_read_ready().unwrap() {
            Async::Ready(r) => r,
            _ => return Async::NotReady,
        };

        // Cache the value
        self.inner.read_readiness.store(super::ready2usize(ready), Relaxed);

        ready.into()
    }

    /// Tests to see if this source is ready to be written to or not.
    ///
    /// If this stream is not ready for a write then `NotReady` will be returned
    /// and the current task will be scheduled to receive a notification when
    /// the stream is writable again. In other words, this method is only safe
    /// to call from within the context of a future's task, typically done in a
    /// `Future::poll` method.
    ///
    /// This is mostly equivalent to `self.poll_ready(Ready::writable())`.
    ///
    /// # Panics
    ///
    /// This function will panic if called outside the context of a future's
    /// task.
    pub fn poll_write(&self) -> Async<()> {
        match self.inner.write_readiness.load(Relaxed) {
            0 => {}
            mut n => {
                // Check what's new with the reactor.
                if let Some(ready) = self.inner.registration.take_write_ready().unwrap() {
                    n |= super::ready2usize(ready);
                    self.inner.write_readiness.store(n, Relaxed);
                }

                return ().into();
            }
        }

        let ready = match self.inner.registration.poll_write_ready().unwrap() {
            Async::Ready(r) => r,
            _ => return Async::NotReady,
        };

        // Cache the value
        self.inner.write_readiness.store(super::ready2usize(ready), Relaxed);

        ().into()
    }

    /// Test to see whether this source fulfills any condition listed in `mask`
    /// provided.
    ///
    /// The `mask` given here is a mio `Ready` set of possible events. This can
    /// contain any events like read/write but also platform-specific events
    /// such as hup and error. The `mask` indicates events that are interested
    /// in being ready.
    ///
    /// If any event in `mask` is ready then it is returned through
    /// `Async::Ready`. The `Ready` set returned is guaranteed to not be empty
    /// and contains all events that are currently ready in the `mask` provided.
    ///
    /// If no events are ready in the `mask` provided then the current task is
    /// scheduled to receive a notification when any of them become ready. If
    /// the `writable` event is contained within `mask` then this
    /// `PollEvented`'s `write` task will be blocked and otherwise the `read`
    /// task will be blocked. This is generally only relevant if you're working
    /// with this `PollEvented` object on multiple tasks.
    ///
    /// # Panics
    ///
    /// This function will panic if called outside the context of a future's
    /// task.
    pub fn poll_ready(&self, mask: Ready) -> Async<Ready> {
        let mut ret = Ready::empty();

        if mask.is_empty() {
            return ret.into();
        }

        if mask.is_writable() {
            if self.poll_write().is_ready() {
                ret = Ready::writable();
            }
        }

        let mask = mask - Ready::writable();

        if !mask.is_empty() {
            if let Async::Ready(v) = self.poll_read2() {
                ret |= v & mask;
            }
        }

        if ret.is_empty() {
            if mask.is_writable() {
                self.need_write();
            }

            if mask.is_readable() {
                self.need_read();
            }

            Async::NotReady
        } else {
            ret.into()
        }
    }

    /// Indicates to this source of events that the corresponding I/O object is
    /// no longer readable, but it needs to be.
    ///
    /// This function, like `poll_read`, is only safe to call from the context
    /// of a future's task (typically in a `Future::poll` implementation). It
    /// informs this readiness stream that the underlying object is no longer
    /// readable, typically because a "would block" error was seen.
    ///
    /// *All* readiness bits associated with this stream except the writable bit
    /// will be reset when this method is called. The current task is then
    /// scheduled to receive a notification whenever anything changes other than
    /// the writable bit. Note that this typically just means the readable bit
    /// is used here, but if you're using a custom I/O object for events like
    /// hup/error this may also be relevant.
    ///
    /// Note that it is also only valid to call this method if `poll_read`
    /// previously indicated that the object is readable. That is, this function
    /// must always be paired with calls to `poll_read` previously.
    ///
    /// # Panics
    ///
    /// This function will panic if called outside the context of a future's
    /// task.
    pub fn need_read(&self) {
        self.inner.read_readiness.store(0, Relaxed);

        if self.poll_read().is_ready() {
            // Notify the current task
            task::current().notify();
        }
    }

    /// Indicates to this source of events that the corresponding I/O object is
    /// no longer writable, but it needs to be.
    ///
    /// This function, like `poll_write`, is only safe to call from the context
    /// of a future's task (typically in a `Future::poll` implementation). It
    /// informs this readiness stream that the underlying object is no longer
    /// writable, typically because a "would block" error was seen.
    ///
    /// The flag indicating that this stream is writable is unset and the
    /// current task is scheduled to receive a notification when the stream is
    /// then again writable.
    ///
    /// Note that it is also only valid to call this method if `poll_write`
    /// previously indicated that the object is writable. That is, this function
    /// must always be paired with calls to `poll_write` previously.
    ///
    /// # Panics
    ///
    /// This function will panic if called outside the context of a future's
    /// task.
    pub fn need_write(&self) {
        self.inner.write_readiness.store(0, Relaxed);

        if self.poll_write().is_ready() {
            // Notify the current task
            task::current().notify();
        }
    }

    /// Returns a reference to the event loop handle that this readiness stream
    /// is associated with.
    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    /// Returns a shared reference to the underlying I/O object this readiness
    /// stream is wrapping.
    pub fn get_ref(&self) -> &E {
        &self.io
    }

    /// Returns a mutable reference to the underlying I/O object this readiness
    /// stream is wrapping.
    pub fn get_mut(&mut self) -> &mut E {
        &mut self.io
    }
}

impl<E: Read> Read for PollEvented<E> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Async::NotReady = PollEvented::poll_read(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_mut().read(buf);

        if is_wouldblock(&r) {
            self.need_read();
        }

        r
    }
}

impl<E: Write> Write for PollEvented<E> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Async::NotReady = PollEvented::poll_write(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_mut().write(buf);

        if is_wouldblock(&r) {
            self.need_write();
        }

        r
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Async::NotReady = PollEvented::poll_write(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_mut().flush();

        if is_wouldblock(&r) {
            self.need_write();
        }

        r
    }
}

impl<E: Read> AsyncRead for PollEvented<E> {
}

impl<E: Write> AsyncWrite for PollEvented<E> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

#[allow(deprecated)]
impl<E: Read + Write> ::io::Io for PollEvented<E> {
    fn poll_read(&mut self) -> Async<()> {
        <PollEvented<E>>::poll_read(self)
    }

    fn poll_write(&mut self) -> Async<()> {
        <PollEvented<E>>::poll_write(self)
    }
}

impl<'a, E> Read for &'a PollEvented<E>
    where &'a E: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Async::NotReady = PollEvented::poll_read(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_ref().read(buf);

        if is_wouldblock(&r) {
            self.need_read();
        }

        r
    }
}

impl<'a, E> Write for &'a PollEvented<E>
    where &'a E: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Async::NotReady = PollEvented::poll_write(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_ref().write(buf);

        if is_wouldblock(&r) {
            self.need_write();
        }

        r
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Async::NotReady = PollEvented::poll_write(self) {
            return Err(io::ErrorKind::WouldBlock.into())
        }

        let r = self.get_ref().flush();

        if is_wouldblock(&r) {
            self.need_write();
        }

        r
    }
}

impl<'a, E> AsyncRead for &'a PollEvented<E>
    where &'a E: Read,
{
}

impl<'a, E> AsyncWrite for &'a PollEvented<E>
    where &'a E: Write,
{
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(().into())
    }
}

#[allow(deprecated)]
impl<'a, E> ::io::Io for &'a PollEvented<E>
    where &'a E: Read + Write,
{
    fn poll_read(&mut self) -> Async<()> {
        <PollEvented<E>>::poll_read(self)
    }

    fn poll_write(&mut self) -> Async<()> {
        <PollEvented<E>>::poll_write(self)
    }
}

fn is_wouldblock<T>(r: &io::Result<T>) -> bool {
    match *r {
        Ok(_) => false,
        Err(ref e) => e.kind() == io::ErrorKind::WouldBlock,
    }
}

impl<E: Evented + fmt::Debug> fmt::Debug for PollEvented<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("PollEvented")
         .field("io", &self.io)
         .finish()
    }
}
