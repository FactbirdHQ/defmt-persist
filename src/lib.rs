//! `defmt` global logger saving to non-volatile storage
//!
//! This is built on the assumption that some persistent storage implements the
//! traits from `embedded-hal::storage`, and that this logger has the full
//! storage capacity from `StorageSize::try_start_address()` and
//! `StorageSize::try_total_size()` words forward.
//!
//! In order to limit this, one can create a newtype wrapper that implements
//! `StorageSize`, returning a subset of the full capacity.
//!

#![no_std]

use bbqueue::{consts, BBBuffer, Consumer, Producer};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};
use cortex_m::{interrupt, register};
use embedded_hal::storage::*;

// TODO: How to make this length more generic?
type LogBufferSize = consts::U256;
type LogQueue = BBBuffer<LogBufferSize>;
type LogProducer = Producer<'static, LogBufferSize>;
type LogConsumer = Consumer<'static, LogBufferSize>;

static mut PRODUCER: Option<LogProducer> = None;

#[defmt::global_logger]
struct Logger;

impl defmt::Write for Logger {
    fn write(&mut self, bytes: &[u8]) {
        match handle().grant_max_remaining(bytes.len()) {
            Ok(mut grant) => {
                let len = grant.buf().len();
                if len < bytes.len() {
                    panic!("LogQueue not big enough!");
                }
                grant.buf().copy_from_slice(&bytes[..len]);
                grant.commit(len);
            }
            Err(_) => {
                panic!("GrantInProgress!");
            }
        }
    }
}

static TAKEN: AtomicBool = AtomicBool::new(false);
static INTERRUPTS_ACTIVE: AtomicBool = AtomicBool::new(false);

unsafe impl defmt::Logger for Logger {
    fn acquire() -> Option<NonNull<dyn defmt::Write>> {
        let primask = register::primask::read();
        interrupt::disable();
        if !TAKEN.load(Ordering::Relaxed) {
            // no need for CAS because interrupts are disabled
            TAKEN.store(true, Ordering::Relaxed);

            INTERRUPTS_ACTIVE.store(primask.is_active(), Ordering::Relaxed);

            Some(NonNull::from(&Logger as &dyn defmt::Write))
        } else {
            if primask.is_active() {
                // re-enable interrupts
                unsafe { interrupt::enable() }
            }
            None
        }
    }

    unsafe fn release(_: NonNull<dyn defmt::Write>) {
        TAKEN.store(false, Ordering::Relaxed);
        if INTERRUPTS_ACTIVE.load(Ordering::Relaxed) {
            // re-enable interrupts
            interrupt::enable()
        }
    }
}

#[inline]
pub fn init(buffer: &'static LogQueue) -> Result<LogDrain, ()> {
    let (prod, cons) = buffer.try_split().map_err(drop)?;
    unsafe { PRODUCER = Some(prod) }
    Ok(LogDrain { inner: cons })
}

/// Returns a reference to the storage.
#[inline]
fn handle() -> &'static mut LogProducer {
    unsafe {
        match PRODUCER {
            Some(ref mut x) => x,
            None => panic!(),
        }
    }
}

pub struct LogDrain {
    inner: LogConsumer,
}

impl LogDrain {
    pub fn drain<S>(&mut self, mut storage: S) -> Result<(), ()>
    where
        S: MultiWrite<u8, u32> + StorageSize<u8, u32>,
    {
        match self.inner.read() {
            Ok(mut grant) => {
                // FIXME: Keep state of address and use `StorageSize` to calculate address every iteration
                nb::block!(storage.try_write_slice(Address(1), grant.buf_mut())).map_err(drop)
            }
            Err(_e) => Err(()),
        }
    }
}
