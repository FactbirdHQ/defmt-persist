use embedded_storage::{ReadStorage, Storage};

pub struct StackStorage<'a> {
    pub buf: &'a mut [u8],
}

impl<'a> ReadStorage for StackStorage<'a> {
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

impl<'a> Storage for StackStorage<'a> {
    fn try_write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        let addr = offset as usize;

        assert!(addr + bytes.len() <= self.buf.len());

        self.buf[addr..addr + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}
