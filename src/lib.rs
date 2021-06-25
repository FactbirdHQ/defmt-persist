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
pub type LogBufferSize = consts::U1024;

pub const MAX_ENCODING_SIZE: usize = 512;

pub type LogBuffer = BBBuffer<LogBufferSize>;

static mut LOGPRODUCER: Option<LogProducer> = None;

pub struct LogProducer {
    producer: Producer<'static, LogBufferSize>,
    encoder: Option<(
        GrantW<'static, LogBufferSize>,
        rzcobs::Encoder<BufWriter<'static>>,
    )>,
}

struct BufWriter<'a> {
    buf: &'a mut [u8],
    i: usize,
}

impl<'a> BufWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, i: 0 }
    }
}

impl<'a> rzcobs::Write for BufWriter<'a> {
    type Error = ();

    fn write(&mut self, byte: u8) -> Result<(), Self::Error> {
        if self.i + 1 >= self.buf.len() {
            return Err(());
        }

        self.buf[self.i] = byte;
        self.i += 1;

        Ok(())
    }
}

impl LogProducer {
    pub fn new(producer: Producer<'static, LogBufferSize>) -> Self {
        Self {
            producer,
            encoder: None,
        }
    }

    pub fn start_encoder(&mut self) -> Result<(), ()> {
        if self.encoder.is_some() {
            return Err(());
        }

        match self.producer.grant_exact(MAX_ENCODING_SIZE) {
            Ok(mut grant) => {
                let buf = unsafe { grant.as_static_mut_buf() };
                self.encoder = Some((grant, rzcobs::Encoder::new(BufWriter::new(buf))));
                Ok(())
            }
            Err(_) => Err(()),
        }
    }

    pub fn encode(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if let Some((_, ref mut encoder)) = self.encoder {
            for b in bytes {
                if let Err(e) = encoder.write(*b) {
                    return Err(e);
                }
            }

            Ok(())
        } else {
            Err(())
        }
    }

    pub fn finalize_encoder(&mut self) -> Result<(), ()> {
        if let Some((mut grant, mut encoder)) = self.encoder.take() {
            let grant_buf = grant.as_mut();

            encoder.end()?;
            let last_encoded_byte = encoder.writer().i;
            let len = last_encoded_byte + 1;
            grant_buf[last_encoded_byte] = 0x00; // Terminator byte has to be written manually

            // Convert 0x00 sentinel into 0xFF sentinel by XORing
            // See `cobs::encode_with_sentinel`
            for b in grant_buf {
                *b ^= COBS_SENTINEL_BYTE;
            }

            grant.commit(len);

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
pub struct StorageHelper<S> {
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
        storage
            .try_read(0, &mut buf)
            .map_err(|_| Error::StorageRead)?;

        Ok(u64::from_be_bytes(buf) == Self::MAGIC_WORD)
    }

    fn write_magic(storage: &mut S) -> Result<(), Error> {
        storage
            .try_write(0, &Self::MAGIC_WORD.to_be_bytes())
            .map_err(|_| Error::StorageWrite)
    }

    fn seek_write_head(storage: &mut S) -> Result<u32, Error> {
        if !Self::check_magic(storage)? {
            return Ok(0);
        }

        // Magic found, let's look for the TWO empty words
        for addr in (WORD_SIZE_BYTES..storage.capacity() as usize).step_by(WORD_SIZE_BYTES) {
            let mut buf = [0u8; WORD_SIZE_BYTES];
            storage
                .try_read(addr as u32, &mut buf)
                .map_err(|_| Error::StorageRead)?;
            let word1 = u64::from_le_bytes(buf);
            storage
                .try_read((addr + WORD_SIZE_BYTES) as u32, &mut buf)
                .map_err(|_| Error::StorageRead)?;
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
        Self::retrieve_frames_helper(&mut self.helper, storage, buf)
    }

    pub fn retrieve_frames_helper(
        helper: &mut StorageHelper<S>,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let read_len = helper.read_slice(storage, buf)?;
        if read_len == 0 {
            return Ok(0);
        }

        let mut frames = buf[..read_len]
            .split(|x| *x == COBS_SENTINEL_BYTE)
            .peekable();
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

                if num_empty_frames >= 8 {
                    helper.decr_read_marker((WORD_SIZE_BYTES - 1) as u32);
                    bytes_written -= WORD_SIZE_BYTES - 1;
                    break;
                }

                helper.incr_read_marker(storage, frame_len as u32);
                bytes_written += frame_len;
            }
        }
        Ok(bytes_written)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::pseudo_flash::PseudoFlashStorage;
    use core::ptr::NonNull;

    fn get_logger<'a>() -> Option<NonNull<dyn defmt::Write>> {
        #[cfg(not(feature = "rtt"))]
        return Some(NonNull::from(&producer::Logger as &dyn defmt::Write));
        #[cfg(feature = "rtt")]
        None
    }

    fn storage_to_str(
        storage: &PseudoFlashStorage,
        sh: &StorageHelper<PseudoFlashStorage>,
        num_bytes: usize,
    ) -> String {
        use std::fmt::Write;

        let mut s = "".to_owned();

        for i in (0..num_bytes).step_by(16) {
            let mut bytes_str = "".to_owned();

            for (bi, byte) in storage.buf[i..(i + 16)].iter().enumerate() {
                write!(bytes_str, "{:02X}", byte).ok();

                let has_write_head = sh.write_head == (i + bi) as u32;
                let has_read_head = sh.read_head == (i + bi) as u32;
                if has_read_head && has_write_head {
                    write!(bytes_str, "b").ok();
                } else if has_write_head {
                    write!(bytes_str, "w").ok();
                } else if has_read_head {
                    write!(bytes_str, "r").ok();
                } else {
                    write!(bytes_str, " ").ok();
                }

                write!(bytes_str, " ").ok();

                if (bi + 1) % WORD_SIZE_BYTES == 0 {
                    write!(bytes_str, "|  ").ok();
                }
            }

            writeln!(s, "{}", bytes_str.trim()).ok();
        }

        s
    }

    fn assert_storage(
        storage: &PseudoFlashStorage,
        sh: &StorageHelper<PseudoFlashStorage>,
        dump: &str,
    ) {
        // Remove starting whitespace
        // This allows for better formatting of the dump in the assertion code
        let dump = dump
            .lines()
            .map(str::trim_start)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        let num_lines = dump.len();
        let dump = dump.join("\n");

        let storage_str = storage_to_str(storage, sh, num_lines * 16);

        if storage_str.trim() != dump.trim() {
            eprintln!("Actual:");
            eprintln!("{}", storage_str);

            eprintln!();
            eprintln!("Expected:");
            eprintln!("{}", dump);

            panic!("Dumps aren't equal");
        }
    }

    #[test]
    fn write_read() {
        let mut storage = PseudoFlashStorage {
            buf: &mut [COBS_SENTINEL_BYTE; 128],
            ..Default::default()
        };

        let mut sh = StorageHelper::try_new(&mut storage).unwrap();

        // 1. Write some bytes but not the whole word (WORD_SIZE_BYTES),
        // so the rest of the word will be filled with COBS_SENTINEL_BYTEs
        sh.write_slice(&mut storage, &[0x00, 0x11, 0x22, 0x33])
            .unwrap();

        // 2. Write the whole word of WORD_SIZE_BYTES bytes,
        // in this case, there shouldn't be any bytes written outside the word boundary
        sh.write_slice(
            &mut storage,
            &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
        )
        .unwrap();

        // 3. Write more than one word of bytes, so it'll consume two words,
        // but the second word will contain only a single byte
        // and the rest of the word will be filled with COBS_SENTINEL_BYTEs
        sh.write_slice(
            &mut storage,
            &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        )
        .unwrap();

        // Assert that data has been written properly and
        // that write head (w) is at the end of the data, and
        // that read head (r) is after the magic number that should be skipped
        assert_storage(
            &storage,
            &sh,
            r#"
                FE  ED  BE  EF  CA  FE  BA  BE  |  00r 11  22  33  FF  FF  FF  FF  |
                00  11  22  33  44  55  66  77  |  00  11  22  33  44  55  66  77  |
                88  FF  FF  FF  FF  FF  FF  FF  |  FFw FF  FF  FF  FF  FF  FF  FF  |
            "#,
        );

        // Check the case #1
        let mut buf = [0u8; 24];
        sh.read_slice(&mut storage, &mut buf[..WORD_SIZE_BYTES])
            .unwrap();
        sh.incr_read_marker(&mut storage, WORD_SIZE_BYTES as u32);
        assert!(matches!(buf, [0x00, 0x11, 0x22, 0x33, 0xFF, ..]));

        // Check the case #2
        sh.read_slice(&mut storage, &mut buf).unwrap();
        sh.incr_read_marker(&mut storage, WORD_SIZE_BYTES as u32);
        assert!(matches!(
            buf,
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, ..]
        ));

        // Check the case #3
        sh.read_slice(&mut storage, &mut buf).unwrap();
        sh.incr_read_marker(&mut storage, (2 * WORD_SIZE_BYTES) as u32);
        assert!(matches!(
            buf,
            [
                0x00,
                0x11,
                0x22,
                0x33,
                0x44,
                0x55,
                0x66,
                0x77,
                0x88,
                0xFF,
                ..
            ]
        ));

        // Both read and write heads are now at the same place (signified by b after FF)
        assert_storage(
            &storage,
            &sh,
            r#"
                FE  ED  BE  EF  CA  FE  BA  BE  |  00  11  22  33  FF  FF  FF  FF  |
                00  11  22  33  44  55  66  77  |  00  11  22  33  44  55  66  77  |
                88  FF  FF  FF  FF  FF  FF  FF  |  FFb FF  FF  FF  FF  FF  FF  FF  |
            "#,
        );
    }

    #[test]
    fn helper_init() {
        let mut storage = PseudoFlashStorage {
            buf: &mut [COBS_SENTINEL_BYTE; 128],
            ..Default::default()
        };

        // This will init the empty storage by
        // 1. Writing the magic word as the first word
        // 2. Writing some data that should go after the magic word
        {
            let mut sh = StorageHelper::try_new(&mut storage).unwrap();
            sh.write_slice(&mut storage, &[0x00, 0x11, 0x22, 0x33])
                .unwrap();
        }

        // Now let's re-initialize the storage helper (to simulate restart)
        let sh = StorageHelper::try_new(&mut storage).unwrap();

        // Assert that data has been written properly and
        // that write head (w) is at the end of the data, and
        // that read head (r) is after the magic number that should be skipped
        assert_storage(
            &storage,
            &sh,
            r#"
                FE  ED  BE  EF  CA  FE  BA  BE  |  00r 11  22  33  FF  FF  FF  FF  |
                FFw FF  FF  FF  FF  FF  FF  FF  |  FF  FF  FF  FF  FF  FF  FF  FF  |
            "#,
        );
    }

    #[test]
    fn log_manager() {
        let mut storage = PseudoFlashStorage {
            buf: &mut [COBS_SENTINEL_BYTE; 256],
            ..Default::default()
        };

        static BUF: LogBuffer = BBBuffer(ConstBBBuffer::new());
        let mut read_data = [0x00; 256];

        let mut log = LogManager::try_new(&BUF, &mut storage).unwrap();

        let frames = [
            vec![0x00; 1],
            vec![0x00; 2],
            vec![0x00; 3],
            vec![0x00; 4],
            vec![0x00; 5],
            vec![0x00; 6],
            vec![0x00; 7],
            vec![0x00; 8],
            vec![0x01, 0x00, 0x01, 0x00],
            vec![0x00, 0x01, 0x00, 0x01],
            vec![0x00, 0x01, 0x00, 0x00, 0x00, 0xFF],
            vec![0xFF, 0x00, 0x00, 0x00, 0x00, 0xFF],
            vec![0xFF_u8; 1],
            vec![0xED; 9],
            vec![0xED; 9],
            vec![0xED; 8],
            vec![0xEE; 7],
            vec![0xEF; 6],
            vec![0xFA; 5],
            vec![0xFB; 4],
            vec![0xFC; 3],
            vec![0xFD; 2],
            vec![0xFE; 1],
        ];

        for frame in frames.iter() {
            handle().start_encoder().unwrap();
            unsafe { get_logger().unwrap().as_mut() }.write(frame);
            handle().finalize_encoder().unwrap();
        }
        log.drain_storage(&mut storage).unwrap();

        let len = log.retrieve_frames(&mut storage, &mut read_data).unwrap();

        let mut num_frames_read = 0;
        for (i, frame) in read_data[..len]
            .split_mut(|b| *b == COBS_SENTINEL_BYTE)
            .filter(|f| f.len() >= 1)
            .enumerate()
        {
            for b in frame.iter_mut() {
                *b ^= 0xFF;
            }
            let frame = rzcobs::decode(frame).unwrap();

            compare_with_trailing_zeros_ignored(&frames[i], &frame);
            num_frames_read += 1;
        }

        assert_eq!(
            num_frames_read,
            frames.len(),
            "Not all of the frames were read"
        );
    }

    // From rzCOBS docs:
    //    When a message is encoded and then decoded, the result is the original message, with up
    //    to 6 zero bytes appended.
    //    Higher layer protocols must be able to deal with these appended zero bytes.
    //
    // Compares two slices of bytes by finding a middle ground w.r.t. number of trailing zeros
    // and cuts the two slices there to have an equal length slices.
    fn compare_with_trailing_zeros_ignored(left: &[u8], right: &[u8]) {
        let zeros_left = left.iter().rev().filter(|b| **b == 0x00).count();
        let zeros_right = right.iter().rev().filter(|b| **b == 0x00).count();
        let zeros_middle = core::cmp::min(zeros_left, zeros_right);

        assert_eq!(left[..zeros_middle], right[..zeros_middle]);
    }
}
