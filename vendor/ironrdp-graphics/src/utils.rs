use core::ops;

use bitvec::prelude::{BitSlice, Msb0};

// FIXME: check if this should be deleted in favor of something else

pub(crate) struct Bits<'a> {
    bits_slice: &'a BitSlice<u8, Msb0>,
}

impl<'a> Bits<'a> {
    pub(crate) fn new(bits_slice: &'a BitSlice<u8, Msb0>) -> Self {
        Self { bits_slice }
    }

    pub(crate) fn split_to(&mut self, at: usize) -> &'a BitSlice<u8, Msb0> {
        let (value, new_bits) = self.bits_slice.split_at(at);
        self.bits_slice = new_bits;
        value
    }
}

impl ops::Deref for Bits<'_> {
    type Target = BitSlice<u8, Msb0>;

    fn deref(&self) -> &Self::Target {
        self.bits_slice
    }
}
