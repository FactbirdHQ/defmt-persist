use embedded_hal::storage::{Address, ReadWrite};

pub struct DummyStorage;

impl ReadWrite for DummyStorage {
    type Error = ();

    fn try_read(&mut self, _address: Address, _bytes: &mut [u8]) -> nb::Result<(), Self::Error> {
        todo!()
    }

    fn try_write(&mut self, _address: Address, _bytes: &[u8]) -> nb::Result<(), Self::Error> {
        todo!()
    }

    fn range(&self) -> (Address, Address) {
        todo!()
    }

    fn try_erase(&mut self) -> nb::Result<(), Self::Error> {
        todo!()
    }
}
