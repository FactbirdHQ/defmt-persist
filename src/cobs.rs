use core::hash::Hasher;

pub struct CobsEncoder<'a> {
    dest: &'a mut [u8],
    dest_idx: usize,
    code_idx: usize,
    num_bt_sent: u8,
    terminate: bool,
    digest: Option<crc::crc16::Digest>,
}

impl<'a> CobsEncoder<'a> {
    /// Create a new streaming Cobs Encoder
    pub fn new(out_buf: &'a mut [u8], crc: bool, terminate: bool) -> CobsEncoder<'a> {
        CobsEncoder {
            dest: out_buf,
            dest_idx: 1,
            code_idx: 0,
            num_bt_sent: 1,
            terminate,
            digest: if crc {
                Some(crc::crc16::Digest::new(0x8408))
            } else {
                None
            },
        }
    }

    /// Push a slice of data to be encoded
    pub fn push(&mut self, data: &[u8]) -> Result<(), ()> {
        // Check if this would fit without iterating through all data
        if self.dest.len() < self.dest_idx + data.len() {
            return Err(());
        }

        for x in data {
            if *x == 0 {
                *self.dest.get_mut(self.code_idx).ok_or_else(|| ())? = self.num_bt_sent;

                self.num_bt_sent = 1;
                self.code_idx = self.dest_idx;
                self.dest_idx += 1;
            } else {
                *self.dest.get_mut(self.dest_idx).ok_or_else(|| ())? = *x;

                self.num_bt_sent += 1;
                self.dest_idx += 1;
                if 0xFF == self.num_bt_sent {
                    *self.dest.get_mut(self.code_idx).ok_or_else(|| ())? = self.num_bt_sent;
                    self.num_bt_sent = 1;
                    self.code_idx = self.dest_idx;
                    self.dest_idx += 1;
                }
            }

            if let Some(ref mut digest) = self.digest {
                digest.write(&[*x]);
            }
        }

        Ok(())
    }

    /// Complete encoding of the output message.
    pub fn finalize(mut self) -> Result<usize, ()> {
        if self.dest_idx == 1 {
            return Ok(0);
        }

        if let Some(digest) = self.digest.take() {
            self.push(&(digest.finish() as u16).to_le_bytes())?;
        }

        // If the current code index is outside of the destination slice,
        // we do not need to write it out
        if let Some(i) = self.dest.get_mut(self.code_idx) {
            *i = self.num_bt_sent;
        }

        if self.terminate {
            *self.dest.get_mut(self.dest_idx).ok_or_else(|| ())? = 0;
            self.dest_idx += 1;
        }

        Ok(self.dest_idx)
    }
}

#[derive(Debug)]
pub struct CobsDecoder<'a> {
    /// Destination slice for decoded message
    dest: &'a mut [u8],

    /// Index of next byte to write in `dest`
    dest_idx: usize,

    /// Decoder state as an enum
    state: DecoderState,
}

#[derive(Debug)]
enum DecoderState {
    /// State machine has not received any non-zero bytes
    Idle,

    /// 1-254 bytes, can be header or 00
    Grab(u8),

    /// 255 bytes, will be a header next
    GrabChain(u8),

    /// Prevent re-using the state machine
    ErrOrComplete,
}

fn add(to: &mut [u8], idx: usize, data: u8) -> Result<(), ()> {
    *to.get_mut(idx).ok_or_else(|| ())? = data;
    Ok(())
}

impl<'a> CobsDecoder<'a> {
    /// Create a new streaming Cobs Decoder. Provide the output buffer
    /// for the decoded message to be placed in
    pub fn new(dest: &'a mut [u8]) -> CobsDecoder<'a> {
        CobsDecoder {
            dest,
            dest_idx: 0,
            state: DecoderState::Idle,
        }
    }

    /// Push a single byte into the streaming CobsDecoder. Return values mean:
    ///
    /// * Ok(None) - State machine okay, more data needed
    /// * Ok(Some(N)) - A message of N bytes was successfully decoded
    /// * Err(M) - Message decoding failed, and M bytes were written to output
    ///
    /// NOTE: Sentinel value must be included in the input to this function for the
    /// decoding to complete
    pub fn feed(&mut self, data: u8) -> Result<Option<usize>, usize> {
        use DecoderState::*;
        let (ret, state) = match (&self.state, data) {
            // Currently Idle, received a terminator, ignore, stay idle
            (Idle, 0x00) => (Ok(None), Idle),

            // Currently Idle, received a byte indicating the
            // next 255 bytes have no zeroes, so we will have 254 unmodified
            // data bytes, then an overhead byte
            (Idle, 0xFF) => (Ok(None), GrabChain(0xFE)),

            // Currently Idle, received a byte indicating there will be a
            // zero that must be modified in the next 1..=254 bytes
            (Idle, n) => (Ok(None), Grab(n - 1)),

            // We have reached the end of a data run indicated by an overhead
            // byte, AND we have recieved the message terminator. This was a
            // well framed message!
            (Grab(0), 0x00) => (Ok(Some(self.dest_idx)), ErrOrComplete),

            // We have reached the end of a data run indicated by an overhead
            // byte, and the next segment of 254 bytes will have no modified
            // sentinel bytes
            (Grab(0), 0xFF) => {
                add(self.dest, self.dest_idx, 0u8).map_err(|_| self.dest_idx)?;
                self.dest_idx += 1usize;
                (Ok(None), GrabChain(0xFE))
            }

            // We have reached the end of a data run indicated by an overhead
            // byte, and we will treat this byte as a modified sentinel byte.
            // place the sentinel byte in the output, and begin processing the
            // next non-sentinel sequence
            (Grab(0), n) => {
                add(self.dest, self.dest_idx, 0u8).map_err(|_| self.dest_idx)?;
                self.dest_idx += 1usize;
                (Ok(None), Grab(n - 1))
            }

            // We were not expecting the sequence to terminate, but here we are.
            // Report an error due to early terminated message
            (Grab(_), 0) => (Err(self.dest_idx), ErrOrComplete),

            // We have not yet reached the end of a data run, decrement the run
            // counter, and place the byte into the decoded output
            (Grab(i), n) => {
                add(self.dest, self.dest_idx, n).map_err(|_| self.dest_idx)?;
                self.dest_idx += 1;
                (Ok(None), Grab(i - 1))
            }

            // We have reached the end of a data run indicated by an overhead
            // byte, AND we have recieved the message terminator. This was a
            // well framed message!
            (GrabChain(0), 0x00) => (Ok(Some(self.dest_idx)), ErrOrComplete),

            // We have reached the end of a data run, and we will begin another
            // data run with an overhead byte expected at the end
            (GrabChain(0), 0xFF) => (Ok(None), GrabChain(0xFE)),

            // We have reached the end of a data run, and we will expect `n` data
            // bytes unmodified, followed by a sentinel byte that must be modified
            (GrabChain(0), n) => (Ok(None), Grab(n - 1)),

            // We were not expecting the sequence to terminate, but here we are.
            // Report an error due to early terminated message
            (GrabChain(_), 0) => (Err(self.dest_idx), ErrOrComplete),

            // We have not yet reached the end of a data run, decrement the run
            // counter, and place the byte into the decoded output
            (GrabChain(i), n) => {
                add(self.dest, self.dest_idx, n).map_err(|_| self.dest_idx)?;
                self.dest_idx += 1;
                (Ok(None), GrabChain(i - 1))
            }

            // When we have errored or successfully completed decoding a message,
            // latch this state to prevent confusion
            (ErrOrComplete, _) => (Err(self.dest_idx), ErrOrComplete),
        };

        self.state = state;
        ret
    }

    /// Push a slice of bytes into the streaming CobsDecoder. Return values mean:
    ///
    /// * Ok(None) - State machine okay, more data needed
    /// * Ok(Some((N, M))) - A message of N bytes was successfully decoded,
    ///     using M bytes from `data` (and earlier data)
    /// * Err(J) - Message decoding failed, and J bytes were written to output
    ///
    /// NOTE: Sentinel value must be included in the input to this function for the
    /// decoding to complete
    pub fn push(&mut self, data: &[u8]) -> Result<Option<(usize, usize)>, usize> {
        for (consumed_idx, d) in data.iter().enumerate() {
            let x = self.feed(*d);
            if let Some(decoded_bytes_ct) = x? {
                // convert from index to number of bytes consumed
                return Ok(Some((decoded_bytes_ct, consumed_idx + 1)));
            }
        }

        Ok(None)
    }
}

// This needs to be a macro because `src` and `dst` could be the same or different.
macro_rules! decode_raw (
    ($src:ident, $dst:ident) => ({
        let mut source_index = 0;
        let mut dest_index = 0;

        while source_index < $src.len() {
            let code = $src[source_index];

            if source_index + code as usize > $src.len() && code != 1 {
                return Err(());
            }

            source_index += 1;

            for _ in 1..code {
                $dst[dest_index] = $src[source_index];
                source_index += 1;
                dest_index += 1;
            }

            if 0xFF != code && source_index < $src.len() {
                $dst[dest_index] = 0;
                dest_index += 1;
            }
        }

        Ok(dest_index)
    })
);

/// Decodes the `source` buffer into the `dest` buffer.
///
/// This function uses the typical sentinel value of 0.
///
/// # Failures
///
/// This will return `Err(())` if there was a decoding error. Otherwise,
/// it will return `Ok(n)` where `n` is the length of the decoded message.
///
/// # Panics
///
/// This function will panic if the `dest` buffer is not large enough for the
/// decoded message. Since an encoded message as always larger than a decoded
/// message, it may be a good idea to make the `dest` buffer as big as the
/// `source` buffer.
pub fn decode(source: &[u8], dest: &mut [u8]) -> Result<usize, ()> {
    let mut dec = CobsDecoder::new(dest);
    assert!(dec.push(source).unwrap().is_none());

    // Explicitly push sentinel of zero
    if let Some((d_used, _s_used)) = dec.push(&[0]).unwrap() {
        Ok(d_used)
    } else {
        Err(())
    }
}

/// Decodes a message in-place.
///
/// This is the same function as `decode`, but replaces the encoded message
/// with the decoded message instead of writing to another buffer.
pub fn decode_in_place(buff: &mut [u8]) -> Result<usize, ()> {
    decode_raw!(buff, buff)
}

/// Calculates the maximum possible size of an encoded message given the length
/// of the source message. This may be useful for calculating how large the
/// `dest` buffer needs to be in the encoding functions.
pub fn max_encoding_length(source_len: usize) -> usize {
    source_len + (source_len / 254) + if source_len % 254 > 0 { 1 } else { 0 }
}
