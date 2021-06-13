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

// We choose the COBS sentinel byte the same as an "empty" memory in FLASH
// This should make it easier to find an empty space in the storage area
const COBS_SENTINEL_BYTE: u8 = 0xFF;

// Typical internal FLASH memory has 64-bit word (8 bytes)
const WORD_SIZE_BYTES: usize = 8;

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

        match self
            .producer
            .grant_exact(cobs::max_encoding_length(len) + 1)
        {
            Ok(mut grant) => {
                let buf = unsafe { grant.as_static_mut_buf() };
                self.encoder = Some((grant, cobs::CobsEncoder::new(buf)));
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
        if let Some((mut grant, encoder)) = self.encoder.take() {
            let used = encoder.finalize()?;

            // Convert 0x00 sentinel into 0xFF sentinel by XORing
            // See `cobs::encode_with_sentinel`
            for b in grant.as_mut() {
                *b ^= COBS_SENTINEL_BYTE;
            }

            grant.commit(used + 1);

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

// Implements a BIP style buffer on top of a physical non-volatile storage,
// implementing `embedded-hal::storage` traits, to be used as persistent log
// storage.
struct StorageHelper<S> {
    read_head: u32,
    write_head: u32,
    _storage: core::marker::PhantomData<S>,
}

pub enum ReadMarker {
    Start,
    Tail,
}

impl<S> StorageHelper<S>
where
    S: Storage,
{
    const MAGIC_WORD: u64 = 0xFEED_BEEF_CAFE_BABE;
    const EMPTY: u64 = 0xFFFF_FFFF_FFFF_FFFF;

    pub fn try_new(storage: &mut S) -> Result<Self, Error> {
        // Check & write magic first
        if !Self::check_magic(storage)? {
            Self::write_magic(storage)?;
        }

        Ok(Self {
            read_head: WORD_SIZE_BYTES as u32, // Skip magic
            write_head: Self::seek_write_head(storage)?,
            _storage: core::marker::PhantomData,
        })
    }

    pub fn write_slice(&mut self, storage: &mut S, data: &[u8]) -> Result<(), Error> {
        let len = data.len();

        if len % WORD_SIZE_BYTES == 0 {
            storage
                .try_write(self.write_head, data)
                .map_err(|_| Error::StorageWrite)?;

            self.write_head += len as u32;
        } else {
            let bytes_within_word = len % WORD_SIZE_BYTES;
            let last_word_index = len.saturating_sub(bytes_within_word);

            // Write words
            if len - bytes_within_word > 0 {
                storage
                    .try_write(self.write_head, &data[..last_word_index])
                    .map_err(|_| Error::StorageWrite)?;
                self.write_head += last_word_index as u32;
            }

            // A small optimization for frequent writes:
            // Avoid writing to the next word if we're currently at the last byte and it's 0xFF
            // this will save us 7 bytes of storage for future writes
            if bytes_within_word == 1 && data[last_word_index] == COBS_SENTINEL_BYTE {
                self.write_head += 1;
            } else {
                let mut word_buf = [COBS_SENTINEL_BYTE; WORD_SIZE_BYTES]; // Fill the word with sentinels
                word_buf[..bytes_within_word].copy_from_slice(&data[last_word_index..]);
                storage
                    .try_write(self.write_head as u32, &word_buf)
                    .map_err(|_| Error::StorageWrite)?;

                self.write_head += WORD_SIZE_BYTES as u32;
            }
        }

        Ok(())
    }

    pub fn read_slice(&mut self, storage: &mut S, data: &mut [u8]) -> Result<usize, Error> {
        if self.read_head == self.write_head {
            return Ok(0);
        }

        let end = storage.capacity() as u32;

        // Handle reading into smaller buffer than the available contigious data
        let len = core::cmp::min(data.len(), end.saturating_sub(self.read_head) as usize);

        storage
            .try_read(self.read_head, &mut data[..len])
            .map_err(|_| Error::StorageRead)?;

        Ok(len)
    }

    pub fn incr_read_marker(&mut self, storage: &mut S, inc: u32) {
        if self.read_head + inc >= storage.capacity() as u32 {
            self.read_head = storage.capacity() as u32;
        }

        self.read_head += inc;
    }

    pub fn decr_read_marker(&mut self, dec: u32) {
        self.read_head = self.read_head.saturating_sub(dec);
    }

    fn check_magic(storage: &mut S) -> Result<bool, Error> {
        let mut buf = [0u8; WORD_SIZE_BYTES];
        storage.try_read(0, &mut buf).map_err(|_| Error::StorageRead)?;

       Ok(u64::from_be_bytes(buf) == Self::MAGIC_WORD)
    }

    fn write_magic(storage: &mut S) -> Result<(), Error> {
        storage
            .try_write(0, &Self::MAGIC_WORD.to_be_bytes())
            .map_err(|_| Error::StorageWrite)
    }

    fn seek_write_head(storage: &mut S) -> Result<u32, Error> {
        if !Self::check_magic(storage)? {
            return Ok(0)
        }

        // Magic found, let's look for the TWO empty words
        for addr in (WORD_SIZE_BYTES..storage.capacity() as usize).step_by(WORD_SIZE_BYTES) {
            let mut buf = [0u8; WORD_SIZE_BYTES];
            storage.try_read(addr as u32, &mut buf).map_err(|_| Error::StorageRead)?;
            let word1 = u64::from_le_bytes(buf);
            storage.try_read((addr + WORD_SIZE_BYTES) as u32, &mut buf).map_err(|_| Error::StorageRead)?;
            let word2 = u64::from_le_bytes(buf);

            if word1 == Self::EMPTY && word2 == Self::EMPTY {
                return Ok(addr as u32);
            }
        }

        Ok(WORD_SIZE_BYTES as u32)
    }
}

pub struct LogManager<S: Storage> {
    inner: Consumer<'static, LogBufferSize>,
    pub(crate) helper: StorageHelper<S>,
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
    /// It's better to call this function as rare as possible as this may help to
    /// use persistent storage memory more optimally.
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
    pub fn retrieve_frames(&mut self, storage: &mut S, buf: &mut [u8]) -> Result<usize, Error> {
        let read_len = self.helper.read_slice(storage, buf)?;
        if read_len == 0 {
            return Ok(0);
        }

        let mut frames = buf[..read_len].split(|x| *x == COBS_SENTINEL_BYTE).peekable();
        let mut bytes_written = 0;
        let mut num_empty_frames = 0;
        while let Some(frame) = frames.next() {
            if frames.peek().is_some() {
                let frame_len = frame.len() + 1;

                if frame_len <= 1 {
                    num_empty_frames += 1;
                } else {
                    num_empty_frames = 0;
                }

                if num_empty_frames >= WORD_SIZE_BYTES {
                    self.helper.decr_read_marker((WORD_SIZE_BYTES - 1) as u32);
                    bytes_written -= WORD_SIZE_BYTES - 1;
                    break;
                }

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
