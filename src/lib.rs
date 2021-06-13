//! `defmt` global logger saving to non-volatile storage
//!
//! This is built on the assumption that some persistent storage implements the
//! traits from `embedded-hal::storage`, and that this logger has the full
//! storage capacity from `StorageSize::try_start_)`and
//! `StorageSize::try_total_size()` words forward.
//!
//! In order to limit this, one can create a newtype wrapper that implements
//! `StorageSize`, returning a subset of the full capacity.
#![cfg_attr(not(test), no_std)]

pub use bbqueue::{consts, BBBuffer, ConstBBBuffer, Consumer, GrantW, Producer};
use core::convert::TryInto;
use embedded_storage::Storage;

#[cfg(test)]
pub mod pseudo_flash;

#[cfg(feature = "rtt")]
pub use defmt_rtt;

#[cfg(not(feature = "rtt"))]
mod producer;

#[derive(Clone, Copy, Debug)]
pub enum Error {
    StorageSize,
    StorageRead,
    StorageWrite,
    BBBuffer,
}

// TODO: How to make this length more generic?
pub type LogBufferSize = consts::U256;

pub type LogBuffer = BBBuffer<LogBufferSize>;

static mut LOGPRODUCER: Option<LogProducer> = None;

pub struct LogProducer {
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

pub unsafe fn set_producer(prod: Producer<'static, LogBufferSize>) {
    LOGPRODUCER = Some(LogProducer::new(prod))
}

/// Returns a reference to the log producer.
#[inline]
pub fn handle() -> &'static mut LogProducer {
    unsafe {
        match LOGPRODUCER {
            Some(ref mut x) => x,
            // Should NEVER happen!
            None => panic!(),
        }
    }
}

// Implements a BIP style buffer ontop of a physical non-volatile storage,
// implementing `embedded-hal::storage` traits, to be used as persistent log
// storage.
struct StorageHelper<S> {
    read_marker: u32,
    header: Header,
    _storage: core::marker::PhantomData<S>,
}

#[derive(Debug, PartialEq)]
struct Header {
    read: u32,
    write: u32,
    watermark: u32,
}

impl Header {
    fn from_storage(buf: [u8; Self::size()], start: u32, end: u32) -> Self {
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

        buf[0..4].copy_from_slice(&self.read.to_le_bytes());
        buf[4..8].copy_from_slice(&self.write.to_le_bytes());
        buf[8..12].copy_from_slice(&self.watermark.to_le_bytes());

        buf
    }

    fn sanity_check_addr(buf: &[u8], start: u32, end: u32) -> u32 {
        if buf.len() != 4 {
            return start;
        }

        match buf[0..4].try_into() {
            Ok(bytes) => {
                let addr = u32::from_le_bytes(bytes);
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
    S: Storage,
{
    pub fn try_new(storage: &mut S) -> Result<Self, Error> {
        let end = storage.capacity();

        // Restore header from storage
        let mut buf = [0u8; Header::size()];
        storage
            .try_read(0, &mut buf)
            .map_err(|_| Error::StorageRead)?;

        let header = Header::from_storage(buf, Header::size() as u32, end as u32);

        Ok(Self {
            read_marker: header.read,
            header,
            _storage: core::marker::PhantomData,
        })
    }

    pub fn write_slice(&mut self, storage: &mut S, data: &mut [u8]) -> Result<(), Error> {
        let end = storage.capacity() as u32;
        let start = Header::size() as u32;

        let len = data.len() as u32;

        // Obtain the address for the first byte of the current write, setting the watermark
        let address = if len > (end - self.header.write) {
            self.header.watermark = self.header.write;
            start
        } else {
            self.header.write
        };

        // In these cases we will need to overwrite existing data by moving read
        if self.header.write < self.header.read && (self.header.read - self.header.write) < len {
            self.header.read = self.header.write + data.len() as u32;
        } else if self.header.write > self.header.read
            && len > (end - self.header.write)
            && len <= (self.header.read - start)
        {
            self.header.read = start + data.len() as u32;
        }

        // Reset watermark if read is incremented above watermark, and set read to start
        if self.header.read >= self.header.watermark {
            self.header.watermark = end;
            self.header.read = start;
        }

        storage
            .try_write(address, data)
            .map_err(|_| Error::StorageWrite)?;

        // Increment the write pointer. now that the data is successfully written
        self.header.write = self.header.write + data.len() as u32;

        // Persist the header
        storage
            .try_write(0, &self.header.to_storage())
            .map_err(|_| Error::StorageWrite)
    }

    pub fn read_slice(&mut self, storage: &mut S, data: &mut [u8]) -> Result<usize, Error> {
        if self.read_marker == self.header.write {
            return Ok(0);
        }

        let end = storage.capacity() as u32;

        let read_end = if self.header.write > self.read_marker {
            self.header.write
        } else {
            core::cmp::min(self.header.watermark, end)
        };

        // Handle reading into smaller buffer than the available contigious data
        let len = core::cmp::min(data.len(), (read_end - self.read_marker) as usize);

        storage
            .try_read(self.read_marker, &mut data[..len])
            .map_err(|_| Error::StorageRead)?;

        Ok(len)
    }

    pub fn incr_read_marker(&mut self, storage: &mut S, len: u32) {
        let start = Header::size() as u32;
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

pub struct LogManager<S: Storage> {
    inner: Consumer<'static, LogBufferSize>,
    helper: StorageHelper<S>,
}

impl<S> LogManager<S>
where
    S: Storage,
{
    /// Initialize a new LogManager.
    ///
    /// This function can only be called once, and will return `Error` if called
    /// multiple times.
    pub fn try_new(
        buffer: &'static BBBuffer<LogBufferSize>,
        storage: &mut S,
    ) -> Result<Self, Error> {
        // NOTE: A `BBBuffer` can only be split once, which makes this function non-reentrant
        match buffer.try_split() {
            Ok((prod, cons)) => {
                unsafe { set_producer(prod) };
                Ok(Self {
                    inner: cons,
                    helper: StorageHelper::try_new(storage)?,
                })
            }
            Err(_e) => Err(Error::BBBuffer),
        }
    }

    /// Drains the log buffer directly into a serial port.
    ///
    /// This function is mainly for debugging purposes.
    ///
    /// **NOTE**: This function is IO-heavy, and should ideally be called only
    /// when the processor is otherwise idle.
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

    /// Drains the log buffer into persistent storage.
    ///
    /// **NOTE**: This function may be IO-heavy, and should ideally be called only
    /// when the processor is otherwise idle.
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

    /// Reads log frames from storage
    ///
    /// Pushes complete log frames into `buf` until a log frame can no longer
    /// fit into `buf, or there is no more complete log frames available in
    /// storage.
    ///
    /// Returns the number of bytes pushed to `buf`
    pub fn retreive_frames(&mut self, storage: &mut S, buf: &mut [u8]) -> Result<usize, Error> {
        let read_len = self.helper.read_slice(storage, buf)?;
        if read_len == 0 {
            return Ok(0);
        }

        let mut frames = buf[..read_len].split_mut(|x| *x == 0).peekable();

        let mut bytes_written = 0;

        while let Some(frame) = frames.next() {
            if frames.peek().is_some() {
                // if frame.is_empty() {
                //     continue;
                // }
                let frame_len = frame.len() + 1;
                self.helper.incr_read_marker(storage, frame_len as u32);
                bytes_written += frame_len;
            }
        }
        Ok(bytes_written)
    }
}

#[cfg(test)]
mod test {
    use crate::pseudo_flash::PseudoFlashStorage;
    use super::*;
    use core::ptr::NonNull;

    fn fill_data(data: &mut [u8]) {
        data.iter_mut()
            .enumerate()
            .map(|(i, x)| *x = i.wrapping_sub(usize::MAX) as u8)
            .count();
    }

    fn get_logger<'a>() -> Option<NonNull<dyn defmt::Write>> {
        #[cfg(not(feature = "rtt"))]
        return Some(NonNull::from(&producer::Logger as &dyn defmt::Write));
        #[cfg(feature = "rtt")]
        None
    }

    #[test]
    pub fn storage_helper() {
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };
        let to = storage.capacity();

        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        let mut write_data = [0u8; 1000];
        let mut read_data = [0u8; 1000];

        fill_data(&mut write_data);

        helper.write_slice(&mut storage, &mut write_data).unwrap();

        assert_eq!(
            &storage.buf[Header::size()..Header::size() + write_data.len()],
            &write_data[..]
        );

        helper.read_slice(&mut storage, &mut read_data).unwrap();

        assert_eq!(&write_data[..], &read_data[..]);
    }

    #[test]
    pub fn dropped_header() {
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };

        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        let mut write_data = [0u8; 1000];
        let mut read_data = [0u8; 1000];

        fill_data(&mut write_data);

        helper.write_slice(&mut storage, &mut write_data).unwrap();

        assert_eq!(
            helper.header,
            Header {
                read: 12,
                write: 1012,
                watermark: 2048
            }
        );

        assert_eq!(
            &storage.buf[Header::size()..Header::size() + write_data.len()],
            &write_data[..]
        );

        // Check that the header is correctly restored from storage
        drop(helper);
        let mut helper = StorageHelper::try_new(&mut storage).unwrap();

        assert_eq!(
            helper.header,
            Header {
                read: 12,
                write: 1012,
                watermark: 2048
            }
        );

        helper.read_slice(&mut storage, &mut read_data).unwrap();
        assert_eq!(&write_data[..], &read_data[..]);
    }

    #[test]
    pub fn log_manager() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };

        let mut write_data = [0u8; 10];
        let mut read_data = [0u8; 14];
        fill_data(&mut write_data);

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);

        log.drain_storage(&mut storage).unwrap();

        let len = log.retreive_frames(&mut storage, &mut read_data).unwrap();
        assert_eq!(&write_data[..], &read_data[1..len - 3]);
    }

    #[test]
    pub fn multiple_frames() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };

        let mut write_data = [0u8; 10];
        let mut read_data = [0u8; 14];
        fill_data(&mut write_data);

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        
        // Write three frames of 10 bytes
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);
        unsafe { get_logger().unwrap().as_mut() }.write(&write_data);
        
        assert_eq!(log.drain_storage(&mut storage).unwrap(), 42);
        dbg!(&storage.buf[0..64]);
        assert_eq!(log.drain_storage(&mut storage).unwrap(), 0);

        for _ in 0..3 {
            let len = log.retreive_frames(&mut storage, &mut read_data).unwrap();
            assert_eq!(&write_data[..], &read_data[1..len - 3]);
        }

        assert_eq!(
            log.retreive_frames(&mut storage, &mut read_data).unwrap(),
            0
        );
    }

    #[test]
    pub fn retreive_multiple_frames() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };
        let to = storage.capacity();

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        unsafe { get_logger().unwrap().as_mut() }.write(&[1, 2, 3, 4]);
        unsafe { get_logger().unwrap().as_mut() }.write(&[5, 6, 7, 8]);
        unsafe { get_logger().unwrap().as_mut() }.write(&[9, 10, 11, 12]);

        assert_eq!(log.drain_storage(&mut storage).unwrap(), 24);
        log.helper
            .write_slice(&mut storage, &mut [7, 13, 14])
            .unwrap();

        let mut read_data = [0u8; 128];

        let len = log.retreive_frames(&mut storage, &mut read_data).unwrap();

        assert_eq!(
            &read_data[..len],
            &[7, 1, 2, 3, 4, 145, 57, 0, 7, 5, 6, 7, 8, 16, 133, 0, 7, 9, 10, 11, 12, 3, 88, 0]
        );
        assert_eq!(len, 24);
        assert_eq!(log.helper.read_marker, (len + Header::size()) as u32);
        {
            let mut read_data = [0u8; 128];
            assert_eq!(
                log.retreive_frames(&mut storage, &mut read_data).unwrap(),
                0
            );
        }
    }

    #[test]
    pub fn retreive_multiple_frames_partially() {
        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut storage = StackStorage {
            buf: &mut [0u8; 2048],
        };
        let to = storage.capacity();

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        unsafe { get_logger().unwrap().as_mut() }.write(&[1, 2, 3, 4]);
        unsafe { get_logger().unwrap().as_mut() }.write(&[5, 6, 7, 8]);
        unsafe { get_logger().unwrap().as_mut() }.write(&[9, 10, 11, 12]);

        assert_eq!(log.drain_storage(&mut storage).unwrap(), 24);
        log.helper
            .write_slice(&mut storage, &mut [7, 13, 14])
            .unwrap();

        {
            let mut read_data = [0u8; 10];
            let len = log.retreive_frames(&mut storage, &mut read_data).unwrap();
            assert_eq!(&read_data[..len], &[7, 1, 2, 3, 4, 145, 57, 0]);
        }

        {
            let mut read_data = [0u8; 128];
            let len = log.retreive_frames(&mut storage, &mut read_data).unwrap();
            assert_eq!(
                &read_data[..len],
                &[7, 5, 6, 7, 8, 16, 133, 0, 7, 9, 10, 11, 12, 3, 88, 0]
            );
        }
        assert_eq!(log.helper.read_marker, 36);

        {
            let mut read_data = [0u8; 128];
            assert_eq!(
                log.retreive_frames(&mut storage, &mut read_data).unwrap(),
                0
            );
        }
    }
}
