pub use bbqueue::{consts, BBBuffer, ConstBBBuffer, Consumer, GrantW, Producer};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};
use cortex_m::{interrupt, register};
use crate::handle;



#[defmt::global_logger]
pub struct Logger;

impl defmt::Write for Logger {
    // fn start_of_frame(&mut self, len: usize) {
    //     handle().start_encoder(len).ok();
    // }

    // fn end_of_frame(&mut self) {
    //     handle().finalize_encoder().ok();
    // }

    fn write(&mut self, bytes: &[u8]) {
        let handle = handle();
        handle.start_encoder(bytes.len()).ok();
        handle.encode(bytes).ok();
        handle.finalize_encoder().ok();
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
