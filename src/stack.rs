use embedded_hal::storage::{Address, ReadWrite};

pub struct StackStorage {
    pub buf: &'static mut [u8],
}

impl ReadWrite for StackStorage {
    type Error = ();

    fn try_read(&mut self, address: Address, bytes: &mut [u8]) -> nb::Result<(), Self::Error> {
        let addr = address.0 as usize;
        defmt::warn!(
            "Reading from log ({:?}-{:?}): {:?}",
            addr,
            bytes.len(),
            &self.buf[addr..addr + bytes.len()]
        );

        // assert!(addr <= self.buf.len());
        // let len = core::cmp::min(self.buf.len() - addr, bytes.len());

        bytes.copy_from_slice(&self.buf[addr..addr + bytes.len()]);
        Ok(())
    }

    fn try_write(&mut self, address: Address, bytes: &[u8]) -> nb::Result<(), Self::Error> {
        let addr = address.0 as usize;

        assert!(addr + bytes.len() <= self.buf.len());

        defmt::warn!("Pushing to log ({:?}): {:?}", addr, bytes);

        self.buf[addr..addr + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn range(&self) -> (Address, Address) {
        (Address(0), Address(self.buf.len() as u32))
    }

    fn try_erase(&mut self, from: Address, to: Address) -> nb::Result<(), Self::Error> {
        self.buf
            .iter_mut()
            .skip(from.0 as usize)
            .take(to.0 as usize)
            .for_each(|x| *x = 1);
        Ok(())
    }
}
