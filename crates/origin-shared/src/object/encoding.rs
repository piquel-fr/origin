//! The building blocks of record encodings. Every field has exactly one
//! encoding, so a record has exactly one too.

use super::DecodeError;

/// A value with a canonical, self-delimiting binary encoding.
///
/// Public only so the crate's public traits can require it: it isn't
/// reachable from outside the crate, which keeps encodings internal.
pub trait Field: Sized {
    /// Appends the encoding to `out`.
    fn write(&self, out: &mut Vec<u8>);

    /// Decodes a value, rejecting any encoding [`Field::write`] wouldn't
    /// produce.
    ///
    /// # Errors
    ///
    /// Fails if the bytes are not a canonical encoding.
    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError>;
}

/// Implements [`Field`] for a struct by encoding its fields in order.
macro_rules! fields {
    ($type:ident { $($field:ident),* $(,)? }) => {
        impl $crate::object::Field for $type {
            fn write(&self, out: &mut Vec<u8>) {
                $($crate::object::Field::write(&self.$field, out);)*
            }

            fn read(
                reader: &mut $crate::object::Reader<'_>,
            ) -> Result<Self, $crate::object::DecodeError> {
                Ok(Self {
                    $($field: $crate::object::Field::read(reader)?,)*
                })
            }
        }
    };
}

/// Implements [`Field`] for an enum whose variants each hold one field, as a
/// tag byte followed by the field.
macro_rules! tagged {
    ($type:ident { $($variant:ident = $tag:literal),* $(,)? }) => {
        impl $crate::object::Field for $type {
            fn write(&self, out: &mut Vec<u8>) {
                match self {
                    $(Self::$variant(value) => {
                        out.push($tag);
                        $crate::object::Field::write(value, out);
                    })*
                }
            }

            fn read(
                reader: &mut $crate::object::Reader<'_>,
            ) -> Result<Self, $crate::object::DecodeError> {
                match reader.u8()? {
                    $($tag => Ok(Self::$variant($crate::object::Field::read(reader)?)),)*
                    tag => Err($crate::object::DecodeError::UnknownTag {
                        field: stringify!($type),
                        tag,
                    }),
                }
            }
        }
    };
}

pub(crate) use {fields, tagged};

/// Little endian.
impl Field for u64 {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_le_bytes());
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        reader.array().map(Self::from_le_bytes)
    }
}

/// One byte, 0 or 1.
impl Field for bool {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(DecodeError::UnknownTag { field: "bool", tag }),
        }
    }
}

/// A `u32` count, then the items.
impl<T: Field> Field for Vec<T> {
    fn write(&self, out: &mut Vec<u8>) {
        Reader::write_len(self.len(), out);
        for item in self {
            item.write(out);
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let len = reader.read_len()?;
        // Each item takes at least a byte: a corrupt count can't make this
        // allocate more than the encoding's size.
        let mut items = Vec::with_capacity(len.min(reader.remaining()));
        for _ in 0..len {
            items.push(T::read(reader)?);
        }
        Ok(items)
    }
}

/// A 0 byte for `None`, or a 1 byte and the value.
impl<T: Field> Field for Option<T> {
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(value) => {
                out.push(1);
                value.write(out);
            }
        }
    }

    fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        match reader.u8()? {
            0 => Ok(None),
            1 => T::read(reader).map(Some),
            tag => Err(DecodeError::UnknownTag {
                field: "option",
                tag,
            }),
        }
    }
}

/// A cursor over an encoding that fails with [`DecodeError::Truncated`]
/// instead of panicking.
pub struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    /// Writes a length as a `u32`. Nothing in a record comes close to
    /// 4 GiB.
    pub(crate) fn write_len(len: usize, out: &mut Vec<u8>) {
        let len = u32::try_from(len).expect("record field longer than 4 GiB");
        out.extend_from_slice(&len.to_le_bytes());
    }

    /// Writes a string as its [`Reader::write_len`] length and its bytes.
    pub(crate) fn write_str(string: &str, out: &mut Vec<u8>) {
        Self::write_len(string.len(), out);
        out.extend_from_slice(string.as_bytes());
    }

    /// Reads a length written by [`Reader::write_len`].
    pub(crate) fn read_len(&mut self) -> Result<usize, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?) as usize)
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < len {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(head)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("took exactly N bytes"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn str_until_nul(&mut self) -> Result<&'a str, DecodeError> {
        let len = self
            .0
            .iter()
            .position(|&byte| byte == 0)
            .ok_or(DecodeError::Truncated)?;
        let name = self.utf8(len)?;
        self.take(1)?;
        Ok(name)
    }

    /// Reads a string written by [`Reader::write_str`].
    pub(crate) fn prefixed_str(&mut self) -> Result<&'a str, DecodeError> {
        let len = self.read_len()?;
        self.utf8(len)
    }

    fn utf8(&mut self, len: usize) -> Result<&'a str, DecodeError> {
        str::from_utf8(self.take(len)?).map_err(|_| DecodeError::InvalidUtf8)
    }

    pub(crate) fn remaining(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}
