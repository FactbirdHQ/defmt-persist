//! `defmt` global logger saving to non-volatile storage
//!
//! This is built on the assumption that some persistent storage implements the
//! traits from `embedded-hal::storage`, and that this logger has the full
//! storage capacity from `StorageSize::try_start_address()` and
//! `StorageSize::try_total_size()` words forward.
//!
//! In order to limit this, one can create a newtype wrapper that implements
//! `StorageSize`, returning a subset of the full capacity.

// #![no_std]

pub use bbqueue::{consts, BBBuffer, ConstBBBuffer, Consumer, GrantW, Producer};
use core::{
    convert::TryInto,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};
use cortex_m::{interrupt, register};
use embedded_hal::storage::{Address, ReadWrite};

mod cobs;
pub mod dummy;

// TODO: How to make this length more generic?
type LogBufferSize = consts::U256;

pub type LogBuffer = BBBuffer<LogBufferSize>;

static mut LOGPRODUCER: Option<LogProducer> = None;

struct LogProducer {
    producer: Producer<'static, LogBufferSize>,
    encoder: Option<(GrantW<'static, LogBufferSize>, cobs::CobsEncoder<'static>)>,
}

impl LogProducer {
    pub fn new(producer: Producer<'static, LogBufferSize>) -> Self {
        Self {
            producer,
            encoder: None,
        }
    }

    pub fn start_encoder(&mut self, len: usize) -> Result<(), ()> {
        if self.encoder.is_some() {
            return Err(());
        }
        // NOTE: `max_encoding_length() + 3` to make room for a 16 bit crc and
        // the sentinel value termination
        match self
            .producer
            .grant_exact(cobs::max_encoding_length(len) + 3)
        {
            Ok(mut grant) => {
                let buf = unsafe { grant.as_static_mut_buf() };
                self.encoder = Some((grant, cobs::CobsEncoder::new(buf, true, true)));
                Ok(())
            }
            Err(_) => Err(()),
        }
    }

    pub fn encode(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if let Some((_, ref mut encoder)) = self.encoder {
            encoder.push(bytes)
        } else {
            Err(())
        }
    }

    pub fn finalize_encoder(&mut self) -> Result<(), ()> {
        if let Some((grant, encoder)) = self.encoder.take() {
            let used = encoder.finalize()?;
            grant.commit(used);
            Ok(())
        } else {
            Err(())
        }
    }
}

#[defmt::global_logger]
struct Logger;

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

#[derive(Clone, Copy, Debug)]
pub enum Error {
    StorageSize,
    StorageRead,
    StorageWrite,
    BBBuffer,
}

// Implements a BIP style buffer ontop of a physical non-volatile storage,
// implementing `embedded-hal::storage` traits, to be used as persistent log
// storage.
struct StorageHelper<S> {
    read_marker: Address,
    header: Header,
    _storage: core::marker::PhantomData<S>,
}

#[derive(Debug, PartialEq)]
struct Header {
    read: Address,
    write: Address,
    watermark: Address,
}

impl Header {
    fn from_storage(buf: [u8; Self::size()], start: Address, end: Address) -> Self {
        Self {
            read: Header::sanity_check_addr(&buf[0..4], start, end),
            write: Header::sanity_check_addr(&buf[4..8], start, end),
            watermark: Header::sanity_check_addr(&buf[8..12], start, end),
        }
    }

    const fn size() -> usize {
        core::mem::size_of::<Header>()
    }

    fn to_storage(&self) -> [u8; Self::size()] {
        let mut buf = [0u8; Self::size()];

        buf[0..4].copy_from_slice(&self.read.0.to_le_bytes());
        buf[4..8].copy_from_slice(&self.write.0.to_le_bytes());
        buf[8..12].copy_from_slice(&self.watermark.0.to_le_bytes());

        buf
    }

    fn sanity_check_addr(buf: &[u8], start: Address, end: Address) -> Address {
        if buf.len() != 4 {
            return start;
        }

        match buf[0..4].try_into() {
            Ok(bytes) => {
                let addr = Address(u32::from_le_bytes(bytes));
                if addr >= start && addr <= end {
                    addr
                } else {
                    start
                }
            }
            Err(_) => start,
        }
    }
}

pub enum ReadMarker {
    Start,
    Tail,
}

impl<S> StorageHelper<S>
where
    S: ReadWrite,
{
    pub fn try_new(storage: &mut S) -> Result<Self, Error> {
        let (start, end) = storage.range();

        // Restore header from storage
        let mut buf = [0u8; Header::size()];
        nb::block!(storage.try_read(start, &mut buf)).map_err(|_| Error::StorageRead)?;

        let header = Header::from_storage(buf, start + Header::size(), end);

        Ok(Self {
            read_marker: header.read,
            header,
            _storage: core::marker::PhantomData,
        })
    }

    pub fn write_slice(&mut self, storage: &mut S, data: &mut [u8]) -> Result<(), Error> {
        let (header_start, end) = storage.range();
        let start = header_start + Header::size();

        let len = data.len() as u32;

        // Obtain the address for the first byte of the current write, setting the watermark
        let address = if len > (end - self.header.write).0 {
            self.header.watermark = self.header.write;
            start
        } else {
            self.header.write
        };

        // In these cases we will need to overwrite existing data by moving read
        if self.header.write < self.header.read && (self.header.read - self.header.write).0 < len {
            self.header.read = self.header.write + data.len();
        } else if self.header.write > self.header.read
            && len > (end - self.header.write).0
            && len <= (self.header.read - start).0
        {
            self.header.read = start + data.len();
        }

        // Reset watermark if read is incremented above watermark, and set read to start
        if self.header.read >= self.header.watermark {
            self.header.watermark = end;
            self.header.read = start;
        }

        nb::block!(storage.try_write(address, data)).map_err(|_| Error::StorageWrite)?;

        // Increment the write pointer. now that the data is successfully written
        self.header.write = self.header.write + data.len();

        // Persist the header
        nb::block!(storage.try_write(header_start, &mut self.header.to_storage()))
            .map_err(|_| Error::StorageWrite)
    }

    pub fn read_slice(&mut self, storage: &mut S, data: &mut [u8]) -> Result<usize, Error> {
        if self.read_marker == self.header.write {
            return Ok(0);
        }

        // Handle reading into smaller buffer than the available contigious data
        let len = core::cmp::min(
            data.len(),
            (self.header.watermark - self.read_marker).0 as usize,
        );

        nb::block!(storage.try_read(self.read_marker, &mut data[..len]))
            .map_err(|_| Error::StorageRead)?;

        Ok(len)
    }

    pub fn incr_read_marker(&mut self, storage: &mut S, len: usize) {
        let start = storage.range().0 + Header::size();
        // Handle wrap around cases
        self.read_marker = if self.read_marker + len < self.header.watermark {
            self.read_marker + len
        } else {
            start
        };

    }

    pub fn set_read_marker(&mut self, position: ReadMarker) {
        self.read_marker = match position {
            ReadMarker::Start => self.header.read,
            ReadMarker::Tail => self.header.write,
        };
    }
}

/// Returns a reference to the log producer.
#[inline]
fn handle() -> &'static mut LogProducer {
    unsafe {
        match LOGPRODUCER {
            Some(ref mut x) => x,
            // Should NEVER happen!
            None => panic!(),
        }
    }
}

pub struct LogManager<S: ReadWrite> {
    inner: Consumer<'static, LogBufferSize>,
    helper: StorageHelper<S>,
}

impl<S> LogManager<S>
where
    S: ReadWrite,
{
    pub fn try_new(
        buffer: &'static BBBuffer<LogBufferSize>,
        storage: &mut S,
    ) -> Result<Self, Error> {
        // NOTE: A `BBBuffer` can only be split once, which makes this function non-reentrant
        match buffer.try_split() {
            Ok((prod, cons)) => {
                unsafe { LOGPRODUCER = Some(LogProducer::new(prod)) }
                Ok(Self {
                    inner: cons,
                    helper: StorageHelper::try_new(storage)?,
                })
            }
            Err(_e) => Err(Error::BBBuffer),
        }
    }

    pub fn drain_serial<W: embedded_hal::serial::Write<u8>>(
        &mut self,
        serial: &mut W,
    ) -> Result<(), ()> {
        match self.inner.read() {
            Ok(mut grant) => {
                let buf = grant.buf_mut();
                let mut frames = buf.split_mut(|x| *x == 0).peekable();
                let mut used = 0;
                if frames.peek().is_some() {
                    while match frames.next() {
                        Some(f) if frames.peek().is_some() => {
                            used += f.len();
                            if let Ok(len) = cobs::decode_in_place(f) {
                                for c in &f[..len] {
                                    nb::block!(serial.try_write(*c)).map_err(|_| ()).ok();
                                }
                                nb::block!(serial.try_flush()).map_err(|_| ()).ok();
                            }
                            true
                        }
                        _ => false,
                    } {}
                } else {
                    // No frames at all!
                }

                grant.release(used);
                Ok(())
            }
            Err(_e) => Err(()),
        }
    }

    pub fn drain_storage(&mut self, storage: &mut S) -> Result<usize, Error> {
        match self.inner.read() {
            Ok(mut grant) => {
                let buf = grant.buf_mut();
                let len = buf.len();
                self.helper.write_slice(storage, buf)?;
                grant.release(len);
                Ok(len)
            }
            Err(bbqueue::Error::InsufficientSize) => Ok(0),
            Err(_) => Err(Error::BBBuffer),
        }
    }

    pub fn retreive_storage(&mut self, storage: &mut S, buf: &mut [u8]) -> Result<usize, Error> {
        let read_len = self.helper.read_slice(storage, buf)?;
        if read_len == 0 {
            return Ok(0);
        }
        let mut frames = buf[..read_len].split_mut(|x| *x == 0);
        if let Some(frame) = frames.next() {
            let frame_len = frame.len();
            if let Ok(len) = cobs::decode_in_place(frame) {
                self.helper.incr_read_marker(storage, frame_len + 1);
                Ok(len)
            } else {
                Ok(0)
            }
        } else {
            // No frames at all!
            Ok(0)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn fill_data(data: &mut [u8]) {
        data.iter_mut()
            .enumerate()
            .map(|(i, x)| *x = i.wrapping_sub(usize::MAX) as u8)
            .count();
    }

    fn get_logger<'a>() -> Option<NonNull<dyn defmt::Write>> {
        Some(NonNull::from(&Logger as &dyn defmt::Write))
    }

    struct TestStorage {
        inner: [u8; 2048],
    }

    impl ReadWrite for TestStorage {
        type Error = ();

        fn try_read(&mut self, address: Address, bytes: &mut [u8]) -> nb::Result<(), Self::Error> {
            let addr = address.0 as usize;

            if addr + bytes.len() > self.inner.len() {
                return Err(nb::Error::Other(()));
            }
            bytes.copy_from_slice(
                &self
                    .inner
                    .get(addr..addr + bytes.len())
                    .ok_or_else(|| nb::Error::Other(()))?,
            );
            Ok(())
        }

        fn try_write(&mut self, address: Address, bytes: &[u8]) -> nb::Result<(), Self::Error> {
            let addr = address.0 as usize;

            let len = if bytes.len()
                > self
                    .inner
                    .get(addr..)
                    .ok_or_else(|| nb::Error::Other(()))?
                    .len()
            {
                // Get max contiguous memory
                self.inner
                    .get(addr..)
                    .ok_or_else(|| nb::Error::Other(()))?
                    .len()
            } else {
                // Get requested length
                bytes.len()
            };

            self.inner
                .get_mut(addr..addr + len)
                .ok_or_else(|| nb::Error::Other(()))?
                .copy_from_slice(bytes);
            Ok(())
        }

        fn range(&self) -> (Address, Address) {
            (Address(0), Address(2048))
        }

        fn try_erase(&mut self) -> nb::Result<(), Self::Error> {
            self.inner.iter_mut().map(|x| *x = 1).count();
            Ok(())
        }
    }

    #[test]
    pub fn storage_helper() {
        let mut storage = TestStorage { inner: [0u8; 2048] };
        storage.try_erase().unwrap();

        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        let mut write_data = [0u8; 1000];
        let mut read_data = [0u8; 1000];

        fill_data(&mut write_data);

        helper.write_slice(&mut storage, &mut write_data).unwrap();

        assert_eq!(
            &storage.inner[Header::size()..Header::size() + write_data.len()],
            &write_data[..]
        );

        helper.read_slice(&mut storage, &mut read_data).unwrap();

        assert_eq!(&write_data[..], &read_data[..]);
    }

    #[test]
    pub fn dropped_header() {
        let mut storage = TestStorage { inner: [0u8; 2048] };
        storage.try_erase().unwrap();

        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        let mut write_data = [0u8; 1000];
        let mut read_data = [0u8; 1000];

        fill_data(&mut write_data);

        helper.write_slice(&mut storage, &mut write_data).unwrap();

        assert_eq!(
            helper.header,
            Header {
                read: Address(12),
                write: Address(1012),
                watermark: Address(2048)
            }
        );

        assert_eq!(
            &storage.inner[Header::size()..Header::size() + write_data.len()],
            &write_data[..]
        );

        // Check that the header is correctly restored from storage
        drop(helper);
        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        assert_eq!(
            helper.header,
            Header {
                read: Address(12),
                write: Address(1012),
                watermark: Address(2048)
            }
        );

        helper.read_slice(&mut storage, &mut read_data).unwrap();
        assert_eq!(&write_data[..], &read_data[..]);
    }

    #[test]
    pub fn log_manager() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = TestStorage { inner: [0u8; 2048] };
        storage.try_erase().unwrap();

        let mut write_data = [0u8; 10];
        let mut read_data = [0u8; 13];
        fill_data(&mut write_data);

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);

        log.drain_storage(&mut storage).unwrap();

        let len = log.retreive_storage(&mut storage, &mut read_data).unwrap();
        assert_eq!(&write_data[..], &read_data[..len - 2]);
    }

    #[test]
    pub fn multiple_frames() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = TestStorage { inner: [0u8; 2048] };
        storage.try_erase().unwrap();

        let mut write_data = [0u8; 10];
        let mut read_data = [0u8; 13];
        fill_data(&mut write_data);

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        // Write three frames of 10 bytes
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);

        assert_eq!(log.drain_storage(&mut storage).unwrap(), 42);
        assert_eq!(log.drain_storage(&mut storage).unwrap(), 0);

        for _ in 0..3 {
            let len = log.retreive_storage(&mut storage, &mut read_data).unwrap();
            assert_eq!(&write_data[..], &read_data[..len - 2]);
        }

        assert_eq!(
            log.retreive_storage(&mut storage, &mut read_data).unwrap(),
            0
        );
    }
}
