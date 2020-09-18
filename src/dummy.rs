use embedded_hal::storage::{ReadWrite, Address};

pub struct DummyStorage;

impl ReadWrite for DummyStorage {
    type Error = ();

    fn try_read(&mut self, address: Address, bytes: &mut [u8]) -> nb::Result<(), Self::Error> {
        todo!()
    }

    fn try_write(&mut self, address: Address, bytes: &[u8]) -> nb::Result<(), Self::Error> {
        todo!()
    }

    fn range(&self) -> (Address, Address) {
        todo!()
    }

    fn try_erase(&mut self) -> nb::Result<(), Self::Error> {
        todo!()
    }
}
