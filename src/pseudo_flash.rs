#![cfg(test)]

use embedded_storage::{ReadStorage, Storage};
use std::collections::HashSet;

#[derive(Default)]
pub struct PseudoFlashStorage<'a> {
    pub buf: &'a mut [u8],
    pub word_set: HashSet<usize>,
}

impl<'a> ReadStorage for PseudoFlashStorage<'a> {
    type Error = ();

    fn try_read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let addr = offset as usize;
        assert!(addr <= self.buf.len());
        bytes.copy_from_slice(&self.buf[addr..addr + bytes.len()]);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }
}

impl<'a> Storage for PseudoFlashStorage<'a> {
    fn try_write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let addr = offset as usize;

        // It isn't allowed by FLASH to write to the same word twice
        for wi in (addr..addr + bytes.len()).step_by(8).map(|a| a / 8) {
            if self.word_set.contains(&wi) {
                return Err(())
            } else {
                self.word_set.insert(wi);
            }
        }

        assert!(addr + bytes.len() <= self.buf.len());

        self.buf[addr..addr + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cant_write_twice() {
        const STORAGE_CAPACITY: usize = 32;
        const WORD_SIZE: usize = 8;

        let mut storage = PseudoFlashStorage {
            buf: &mut [0xFFu8; STORAGE_CAPACITY],
            ..Default::default()
        };

        for wi in (0..STORAGE_CAPACITY).step_by(WORD_SIZE) {
            storage.try_write(wi as u32, &[0xFF; WORD_SIZE]).unwrap();

            // FLASH won't allow to write to the same programmed word twice before erasing
            assert!(storage.try_write(wi as u32, &[0xFF; WORD_SIZE]).is_err());
        }

        // The code above should touch all of the storage words
        assert_eq!(storage.word_set.len(), STORAGE_CAPACITY / WORD_SIZE);
    }
}