#![cfg(test)]

use embedded_storage::{ErasableStorage, ReadStorage, Storage};
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
                return Err(());
            } else {
                self.word_set.insert(wi);
            }
        }

        assert!(addr + bytes.len() <= self.buf.len());

        self.buf[addr..addr + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

impl<'a> ErasableStorage for PseudoFlashStorage<'a> {
    type Error = ();

    const ERASE_SIZE: u32 = 128;

    fn try_erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        if from % Self::ERASE_SIZE != 0 {
            return Err(()); // Not aligned
        }

        if to >= self.capacity() as u32 {
            return Err(()); // Overflow
        }

        for page_addr in (from..=to).step_by(Self::ERASE_SIZE as usize) {
            let page_addr = page_addr as usize;
            self.buf[page_addr..(page_addr + Self::ERASE_SIZE as usize)].fill(0xFF);

            for wi in (page_addr..(page_addr + Self::ERASE_SIZE as usize))
                .step_by(8)
                .map(|a| a / 8)
            {
                self.word_set.remove(&wi);
            }
        }

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
            storage.try_write(wi as u32, &[0x00; WORD_SIZE]).unwrap();

            // FLASH won't allow to write to the same programmed word twice before erasing
            assert!(storage.try_write(wi as u32, &[0x00; WORD_SIZE]).is_err());
        }

        // The code above should touch all of the storage words
        assert_eq!(storage.word_set.len(), STORAGE_CAPACITY / WORD_SIZE);
    }

    #[test]
    fn can_write_after_erase() {
        const NUM_PAGES: usize = 4;
        const PAGE_SIZE: usize = PseudoFlashStorage::ERASE_SIZE as usize;
        const STORAGE_CAPACITY: usize = PAGE_SIZE * NUM_PAGES;

        let mut storage = PseudoFlashStorage {
            buf: &mut [0xFFu8; STORAGE_CAPACITY],
            ..Default::default()
        };

        for page_nr in (0..NUM_PAGES).into_iter() {
            let from = (PAGE_SIZE * page_nr) as u32;
            let to = (PAGE_SIZE * (page_nr + 1)) as u32;

            storage.try_write(from, &[0x00; PAGE_SIZE]).unwrap();
            assert!(
                storage.try_write(from, &[0x00; PAGE_SIZE]).is_err(),
                "can't write before erased"
            );
            assert!(
                storage.try_erase(from, to - 1).is_ok(),
                "can write after erase"
            );
            storage.try_write(from, &[0x00; PAGE_SIZE]).unwrap();
            assert!(
                storage.try_write(from, &[0x00; PAGE_SIZE]).is_err(),
                "can't write before erased"
            );

            assert!(
                storage.try_erase(from + 1, to).is_err(),
                "can erase unaligned page"
            );
        }
    }
}
