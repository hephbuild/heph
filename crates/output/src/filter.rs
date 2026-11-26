//! A `Serializer` wrapper that drops the fields `--fields` did not select.
//!
//! It sits between a record's `Serialize` impl and serde_json's serializer:
//! `serialize_struct` passes through, and each `serialize_field` either
//! forwards or becomes `skip_field`. No `Value` is built per row and the key
//! order is the record's. `skip_serializing_if` on an `Option` could not do
//! this: it cannot tell "not selected" from "selected and null".

use crate::{Field, Selection};
use serde::Serialize;
use serde::ser::{Error as _, Impossible, SerializeStruct, Serializer};

/// Serializes a [`crate::Record`] with only the `selection` of `fields`.
pub(crate) struct Filtered<S> {
    pub(crate) inner: S,
    pub(crate) selection: Selection,
    pub(crate) fields: &'static [Field],
}

/// The error for anything but a struct, which `derive(Record)` rules out.
fn not_a_struct<E: serde::ser::Error>() -> E {
    E::custom("a Record serializes as a struct")
}

macro_rules! refuse {
    ($($method:ident($($arg:ty),*);)*) => {
        $(
            fn $method(self, $(_: $arg),*) -> Result<Self::Ok, Self::Error> {
                Err(not_a_struct())
            }
        )*
    };
}

impl<S: Serializer> Serializer for Filtered<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Impossible<S::Ok, S::Error>;
    type SerializeTuple = Impossible<S::Ok, S::Error>;
    type SerializeTupleStruct = Impossible<S::Ok, S::Error>;
    type SerializeTupleVariant = Impossible<S::Ok, S::Error>;
    type SerializeMap = Impossible<S::Ok, S::Error>;
    type SerializeStruct = FilteredStruct<S::SerializeStruct>;
    type SerializeStructVariant = Impossible<S::Ok, S::Error>;

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        // `len` passes through unchanged: serde_json closes a struct it is
        // told has no fields at once, and serde's count is an upper bound
        // of what is written either way.
        Ok(FilteredStruct {
            inner: self.inner.serialize_struct(name, len)?,
            selection: self.selection,
            fields: self.fields,
        })
    }

    refuse! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_f32(f32);
        serialize_f64(f64);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
        serialize_none();
        serialize_unit();
        serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<Self::Ok, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Err(not_a_struct())
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(not_a_struct())
    }
}

pub(crate) struct FilteredStruct<S> {
    inner: S,
    selection: Selection,
    fields: &'static [Field],
}

impl<S: SerializeStruct> SerializeStruct for FilteredStruct<S> {
    type Ok = S::Ok;
    type Error = S::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        // I8 (keys ⊆ FIELDS) holds by construction of the derive; a key
        // missing here is a bug, reported rather than printed.
        let Some(index) = self.fields.iter().position(|f| f.name == key) else {
            return Err(S::Error::custom(format_args!(
                "key `{key}` is not in the record's field table"
            )));
        };
        if self.selection.contains(index) {
            self.inner.serialize_field(key, value)
        } else {
            self.inner.skip_field(key)
        }
    }

    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.inner.skip_field(key)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}
