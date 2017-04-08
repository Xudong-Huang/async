// {{{ Crate docs
//! # Async drain for slog-rs
//!
//! `slog-rs` is an ecosystem of reusable components for structured, extensible,
//! composable logging for Rust.
//!
//! `slog-async` allows building `Drain`s that offload processing to another
//! thread.  Typically serialization and IO operations can be slow enough that
//! they could make logging hinder performance of the main code. Sending logging
//! records to another thread is much faster (ballpark of 100ns).
//!
//! Note: Unlike other logging solutions `slog-rs` does not have a hardcoded
//! async logging implementation. This crate is just a reasonable reference
//! implementation. It should have good performance and work well in most use
//! cases. See documentation and implementation for more details.
//!
//! It's relatively easy to implement own `slog-rs` async logging. Feel free to
//! use this one as a starting point.
// }}}

// {{{ Imports & meta
#![warn(missing_docs)]

#[macro_use]
extern crate slog;
extern crate thread_local;
extern crate take_mut;

use slog::{Record, RecordStatic, Level, SingleKV, KV, BorrowedKV};
use slog::{Serializer, OwnedKVList, Key};

use slog::Drain;
use std::{io, thread};
use std::error::Error;
use std::fmt;
use std::sync;

use std::sync::{mpsc, Mutex};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use take_mut::take;
// }}}

// {{{ Serializer
struct ToSendSerializer {
    kv: Box<KV + Send>,
}

impl ToSendSerializer {
    fn new() -> Self {
        ToSendSerializer { kv: Box::new(()) }
    }

    fn finish(self) -> Box<KV + Send> {
        self.kv
    }
}

impl Serializer for ToSendSerializer {
    fn emit_bool(&mut self, key: Key, val: bool) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_unit(&mut self, key: Key) -> slog::Result {
        let val = ();
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_none(&mut self, key: Key) -> slog::Result {
        let val: Option<()> = None;
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_char(&mut self, key: Key, val: char) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_u8(&mut self, key: Key, val: u8) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_i8(&mut self, key: Key, val: i8) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_u16(&mut self, key: Key, val: u16) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_i16(&mut self, key: Key, val: i16) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_u32(&mut self, key: Key, val: u32) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_i32(&mut self, key: Key, val: i32) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_f32(&mut self, key: Key, val: f32) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_u64(&mut self, key: Key, val: u64) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_i64(&mut self, key: Key, val: i64) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_f64(&mut self, key: Key, val: f64) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_usize(&mut self, key: Key, val: usize) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_isize(&mut self, key: Key, val: isize) -> slog::Result {
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_str(&mut self, key: Key, val: &str) -> slog::Result {
        let val = val.to_owned();
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
    fn emit_arguments(&mut self,
                      key: Key,
                      val: &fmt::Arguments)
                      -> slog::Result {
        let val = fmt::format(*val);
        take(&mut self.kv, |kv| Box::new((SingleKV(key, val), kv)));
        Ok(())
    }
}
// }}}

// {{{ Async
// {{{ AsyncError
/// Errors reported by `Async`
#[derive(Debug)]
pub enum AsyncError {
    /// Could not send record to worker thread due to full queue
    Full,
    /// Fatal problem - mutex or channel poisoning issue
    Fatal(Box<std::error::Error>),
}

impl<T> From<mpsc::TrySendError<T>> for AsyncError {
    fn from(_: mpsc::TrySendError<T>) -> AsyncError {
        AsyncError::Full
    }
}
impl<T> From<std::sync::TryLockError<T>> for AsyncError {
    fn from(_: std::sync::TryLockError<T>) -> AsyncError {
        AsyncError::Full
    }
}


impl<T> From<std::sync::PoisonError<T>> for AsyncError {
    fn from(err: std::sync::PoisonError<T>) -> AsyncError {
        AsyncError::Fatal(Box::new(io::Error::new(io::ErrorKind::BrokenPipe,
                                                  err.description())))
    }
}
/// `AsyncResult` alias
pub type AsyncResult<T> = std::result::Result<T, AsyncError>;

// }}}

// {{{ AsyncCore
/// `AsyncCore` builder
pub struct AsyncCoreBuilder<D>
    where D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static
{
    chan_size: usize,
    drain: D,
}

impl<D> AsyncCoreBuilder<D>
    where D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static
{
    fn new(drain: D) -> Self {
        AsyncCoreBuilder {
            chan_size: 128,
            drain: drain,
        }
    }

    /// Set channel size used to send logging records to worker thread. When
    /// buffer is full `AsyncCore` will start returning `AsyncError::Full`.
    pub fn chan_size(mut self, s: usize) -> Self {
        self.chan_size = s;
        self
    }

    /// Build `AsyncCore`
    pub fn build(self) -> AsyncCore {
        let (tx, rx) = mpsc::sync_channel(self.chan_size);
        let join = thread::spawn(move || loop {
            match rx.recv().unwrap() {
                AsyncMsg::Record(r) => {
                    let rs = RecordStatic {
                        location: &*r.location,
                        level: r.level,
                        tag: &r.tag,
                    };

                    self.drain
                        .log(&Record::new(&rs,
                                          &format_args!("{}", r.msg),
                                          BorrowedKV(&r.kv)),
                             &r.logger_values)
                        .unwrap();
                }
                AsyncMsg::Finish => return,
            }
        });

        AsyncCore {
            ref_sender: Mutex::new(tx),
            tl_sender: thread_local::ThreadLocal::new(),
            join: Mutex::new(Some(join)),
        }
    }
}

/// Core of `Async` drain
///
/// See `Async` for documentation.
///
/// Wrapping `AsyncCore` allows implementing custom overflow (and other errors)
/// handling strategy.
///
/// Note: On drop `AsyncCore` waits for it's worker-thread to finish (after handling
/// all previous `Record`s sent to it). If you can't tolerate the delay, make
/// sure you drop it eg. in another thread.
pub struct AsyncCore {
    ref_sender: Mutex<mpsc::SyncSender<AsyncMsg>>,
    tl_sender: thread_local::ThreadLocal<mpsc::SyncSender<AsyncMsg>>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl AsyncCore {
    /// New `AsyncCore` with default parameters
    pub fn new<D>(drain: D) -> Self
        where D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static,
              D: std::panic::RefUnwindSafe
    {
        AsyncCoreBuilder::new(drain).build()
    }

    /// Build `AsyncCore` drain with custom parameters
    pub fn custom<D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static>
        (drain: D)
         -> AsyncCoreBuilder<D> {
        AsyncCoreBuilder::new(drain)
    }
    fn get_sender(&self)
                  -> Result<&mpsc::SyncSender<AsyncMsg>,
                            std::sync::PoisonError<sync::MutexGuard<mpsc::SyncSender<AsyncMsg>>>> {
        self.tl_sender
            .get_or_try(|| Ok(Box::new(self.ref_sender.lock()?.clone())))
    }

    /// Send `AsyncRecord` to a worker thread.
    fn send(&self, r: AsyncRecord) -> AsyncResult<()> {
        let sender = self.get_sender()?;

        sender.try_send(AsyncMsg::Record(r))?;

        Ok(())
    }
}

impl Drain for AsyncCore {
    type Ok = ();
    type Err = AsyncError;

    fn log(&self,
           record: &Record,
           logger_values: &OwnedKVList)
           -> AsyncResult<()> {

        let mut ser = ToSendSerializer::new();
        record.kv()
            .serialize(record, &mut ser)
            .expect("`ToSendSerializer` can't fail");

        self.send(AsyncRecord {
            msg: fmt::format(*record.msg()),
            level: record.level(),
            location: Box::new(*record.location()),
            tag: String::from(record.tag()),
            logger_values: logger_values.clone(),
            kv: ser.finish(),
        })
    }
}

struct AsyncRecord {
    msg: String,
    level: Level,
    location: Box<slog::RecordLocation>,
    tag: String,
    logger_values: OwnedKVList,
    kv: Box<KV + Send>,
}

enum AsyncMsg {
    Record(AsyncRecord),
    Finish,
}

impl Drop for AsyncCore {
    fn drop(&mut self) {
        let _err: Result<(), Box<std::error::Error>> = {
            || {
                let _ = self.get_sender()?.send(AsyncMsg::Finish);
                self.join
                    .lock()?
                    .take()
                    .unwrap()
                    .join()
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe,
                                       "Logging thread worker join error")
                    })?;

                Ok(())
            }
        }();
    }
}
// }}}

/// `Async` builder
pub struct AsyncBuilder<D>
    where D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static
{
    core: AsyncCoreBuilder<D>,
}

impl<D> AsyncBuilder<D>
    where D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static
{
    fn new(drain: D) -> AsyncBuilder<D> {
        AsyncBuilder { core: AsyncCoreBuilder::new(drain) }
    }

    /// Set channel size used to send logging records to worker thread. When
    /// buffer is full `AsyncCore` will start returning `AsyncError::Full`.
    pub fn chan_size(self, s: usize) -> Self {
        AsyncBuilder { core: self.core.chan_size(s) }
    }

    /// Complete building `Async`
    pub fn build(self) -> Async {
        Async {
            core: self.core.build(),
            dropped: AtomicUsize::new(0),
        }
    }
}

/// Async drain
///
/// `Async` will send all the logging records to a wrapped drain running in
/// another thread.
///
/// On `AsyncError::Full` returned by `AsyncCore` used internally, `Async` will
/// drop overflowing `Records` and report number of dropped messages.
///
/// Note: On drop `Async` waits for it's worker-thread to finish (after handling
/// all previous `Record`s sent to it). If you can't tolerate the delay, make
/// sure you drop it eg. in another thread.
pub struct Async {
    core: AsyncCore,
    dropped: AtomicUsize,
}

impl Async {
    /// New `AsyncCore` with default parameters
    pub fn default<D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static>
        (drain: D)
         -> Self {
        AsyncBuilder::new(drain).build()
    }

    /// Build `Async` drain with custom parameters
    ///
    /// The wrapped drain must handle all results (`Drain<Ok=(),Error=Never>`)
    /// since there's no way to return it back. See `slog::DrainExt::fuse()` and
    /// `slog::DrainExt::ignore_res()` for typical error handling strategies.
    pub fn new<D: slog::Drain<Err = slog::Never, Ok = ()> + Send + 'static>
        (drain: D)
         -> AsyncBuilder<D> {
        AsyncBuilder::new(drain)
    }

    fn push_dropped(&self, logger_values: &OwnedKVList) -> AsyncResult<()> {
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            match self.core.log(&record!(slog::Level::Error,
                                         "slog-async",
                                         &format_args!("dropped messages \
                                                        due to channel \
                                                        overflow"),
                                         b!("count" => dropped)),
                                logger_values) {
                Ok(()) => {}
                Err(AsyncError::Full) => {
                    self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Drain for Async {
    type Ok = ();
    type Err = AsyncError;

    // TODO: Review `Ordering::Relaxed`
    fn log(&self,
           record: &Record,
           logger_values: &OwnedKVList)
           -> AsyncResult<()> {

        self.push_dropped(logger_values)?;

        match self.core.log(record, logger_values) {
            Ok(()) => {}
            Err(AsyncError::Full) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        Ok(())
    }
}

impl Drop for Async {
    fn drop(&mut self) {
        let _ = self.push_dropped(&o!().into());
    }
}

// }}}

// vim: foldmethod=marker foldmarker={{{,}}}
