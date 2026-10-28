//! Strings restricted to valid names: file names, target names, statement
//! kinds and principals.

/// Defines string types holding only valid names. Each type defines
/// `fn is_valid(name: &str) -> bool` in its own `impl` block, where
/// constants can also borrow a static name: `Self(Cow::Borrowed("main"))`.
///
/// Names are encoded as a `u32` length and their UTF-8 bytes.
macro_rules! names {
    ($($(#[$meta:meta])* $name:ident($what:literal);)*) => {$(
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(std::borrow::Cow<'static, str>);

        impl $name {
            /// Validates a name.
            ///
            /// # Errors
            ///
            /// Fails if the name breaks the rules above.
            pub fn new(name: impl Into<String>) -> Result<Self, $crate::InvalidName> {
                let name = name.into();
                if Self::is_valid(&name) {
                    Ok(Self(std::borrow::Cow::Owned(name)))
                } else {
                    Err($crate::InvalidName { what: $what, name })
                }
            }

            /// The name as a string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::InvalidName;

            fn from_str(name: &str) -> Result<Self, Self::Err> {
                Self::new(name)
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&*self.0, f)
            }
        }

        impl $crate::object::Field for $name {
            fn write(&self, out: &mut Vec<u8>) {
                $crate::object::Reader::write_str(&self.0, out);
            }

            fn read(
                reader: &mut $crate::object::Reader<'_>,
            ) -> Result<Self, $crate::object::DecodeError> {
                Ok(Self::new(reader.prefixed_str()?)?)
            }
        }
    )*};
}

pub(crate) use names;

/// A string that is not a valid name of its kind.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what} {name:?}")]
pub struct InvalidName {
    /// What the name was for, such as "file name".
    pub what: &'static str,
    /// The rejected name.
    pub name: String,
}
