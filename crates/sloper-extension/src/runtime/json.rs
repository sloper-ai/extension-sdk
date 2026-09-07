//! Bounded JSON encoding that preserves item number rules.

use std::{
    cell::Cell,
    fmt,
    io,
};

use serde::{
    Serialize,
    Serializer,
    ser::{
        Error as _,
        SerializeMap,
        SerializeSeq,
        SerializeStruct,
        SerializeStructVariant,
        SerializeTuple,
        SerializeTupleStruct,
        SerializeTupleVariant,
    },
};

use crate::{
    Error,
    Result,
};

// Bound each encoded item separately from the 8 MiB batch.
const MAX_BYTES: usize = 1024 * 1024;
const NONFINITE_NUMBER: &str = "resource numbers must be finite";
const ENCODING_FAILED: &str = "resource serialization failed";

pub(super) fn encode<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    let nonfinite = Cell::new(false);
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        exceeded: false,
    };
    let mut serializer = serde_json::Serializer::new(&mut output);
    let result = value.serialize(FiniteSerializer(&mut serializer, &nonfinite));
    // These conditions remain failures when authored Serialize code discards
    // an element's error and goes on to finish its enclosing collection.
    if output.exceeded {
        return Err(Error::TooLarge);
    }
    if nonfinite.get() {
        return Err(Error::internal(ENCODING_FAILED));
    }
    // Custom diagnostics can contain item data and are redacted here, at the
    // boundary between authored serialization code and the guest runtime.
    result.map_err(|_| Error::internal(ENCODING_FAILED))?;
    String::from_utf8(output.bytes).map_err(|_| Error::internal(ENCODING_FAILED))
}

struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl io::Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(Error::TooLarge.into());
        }
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() {
            // Reserve geometrically without allowing Vec's growth strategy
            // to allocate beyond the wire limit.
            let capacity = required.next_power_of_two().min(MAX_BYTES);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FiniteValue<'a, T: ?Sized>(&'a T, &'a Cell<bool>);

impl<T: Serialize + ?Sized> Serialize for FiniteValue<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(FiniteSerializer(serializer, self.1))
    }
}

struct FiniteSerializer<'a, S>(S, &'a Cell<bool>);

impl<'a, S: Serializer> Serializer for FiniteSerializer<'a, S> {
    type Error = S::Error;
    type Ok = S::Ok;
    type SerializeMap = FiniteCompound<'a, S::SerializeMap>;
    type SerializeSeq = FiniteCompound<'a, S::SerializeSeq>;
    type SerializeStruct = FiniteCompound<'a, S::SerializeStruct>;
    type SerializeStructVariant = FiniteCompound<'a, S::SerializeStructVariant>;
    type SerializeTuple = FiniteCompound<'a, S::SerializeTuple>;
    type SerializeTupleStruct = FiniteCompound<'a, S::SerializeTupleStruct>;
    type SerializeTupleVariant = FiniteCompound<'a, S::SerializeTupleVariant>;

    fn serialize_bool(self, value: bool) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_bool(value)
    }

    fn serialize_i8(self, value: i8) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_i8(value)
    }

    fn serialize_i16(self, value: i16) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_i16(value)
    }

    fn serialize_i32(self, value: i32) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_i32(value)
    }

    fn serialize_i64(self, value: i64) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_i64(value)
    }

    fn serialize_i128(self, value: i128) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_i128(value)
    }

    fn serialize_u8(self, value: u8) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_u8(value)
    }

    fn serialize_u16(self, value: u16) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_u16(value)
    }

    fn serialize_u32(self, value: u32) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_u32(value)
    }

    fn serialize_u64(self, value: u64) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_u64(value)
    }

    fn serialize_u128(self, value: u128) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_u128(value)
    }

    fn serialize_f32(self, value: f32) -> std::result::Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            self.1.set(true);
            return Err(S::Error::custom(NONFINITE_NUMBER));
        }
        self.0.serialize_f32(value)
    }

    fn serialize_f64(self, value: f64) -> std::result::Result<Self::Ok, Self::Error> {
        if !value.is_finite() {
            self.1.set(true);
            return Err(S::Error::custom(NONFINITE_NUMBER));
        }
        self.0.serialize_f64(value)
    }

    fn serialize_char(self, value: char) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_char(value)
    }

    fn serialize_str(self, value: &str) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_str(value)
    }

    fn serialize_bytes(self, value: &[u8]) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_bytes(value)
    }

    fn serialize_none(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&FiniteValue(value, self.1))
    }

    fn serialize_unit(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }

    fn serialize_unit_struct(self, name: &'static str) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &FiniteValue(value, self.1))
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &FiniteValue(value, self.1))
    }

    fn serialize_seq(self, len: Option<usize>) -> std::result::Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_tuple(self, len: usize) -> std::result::Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeTupleStruct, Self::Error> {
        self.0
            .serialize_tuple_struct(name, len)
            .map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_map(self, len: Option<usize>) -> std::result::Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeStruct, Self::Error> {
        self.0
            .serialize_struct(name, len)
            .map(|value| FiniteCompound(value, self.1))
    }

    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> std::result::Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(|value| FiniteCompound(value, self.1))
    }

    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.collect_str(value)
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

struct FiniteCompound<'a, S>(S, &'a Cell<bool>);

impl<S: SerializeSeq> SerializeSeq for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_element(&FiniteValue(value, self.1))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTuple> SerializeTuple for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_element(&FiniteValue(value, self.1))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTupleStruct> SerializeTupleStruct for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_field(&FiniteValue(value, self.1))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeTupleVariant> SerializeTupleVariant for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_field(&FiniteValue(value, self.1))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeMap> SerializeMap for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_key(&FiniteValue(key, self.1))
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> std::result::Result<(), Self::Error> {
        self.0.serialize_value(&FiniteValue(value, self.1))
    }

    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(
        &mut self,
        key: &K,
        value: &V,
    ) -> std::result::Result<(), Self::Error> {
        self.0
            .serialize_entry(&FiniteValue(key, self.1), &FiniteValue(value, self.1))
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeStruct> SerializeStruct for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        self.0.serialize_field(key, &FiniteValue(value, self.1))
    }

    fn skip_field(&mut self, key: &'static str) -> std::result::Result<(), Self::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<S: SerializeStructVariant> SerializeStructVariant for FiniteCompound<'_, S> {
    type Error = S::Error;
    type Ok = S::Ok;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        self.0.serialize_field(key, &FiniteValue(value, self.1))
    }

    fn skip_field(&mut self, key: &'static str) -> std::result::Result<(), Self::Error> {
        self.0.skip_field(key)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        collections::BTreeMap,
        io::Write as _,
    };

    use serde::{
        Serialize,
        Serializer,
        ser::{
            Error as _,
            SerializeSeq as _,
        },
    };

    use super::{
        BoundedOutput,
        ENCODING_FAILED,
        MAX_BYTES,
        encode,
    };
    use crate::Error;

    #[test]
    fn finite_values_retain_json_representation() {
        #[derive(Serialize)]
        struct Item {
            number: f64,
            optional: Option<f64>,
            absent: Option<f64>,
            list: Vec<f64>,
            fields: BTreeMap<String, f64>,
        }

        let item = Item {
            number: -12.5,
            optional: Some(0.0),
            absent: None,
            list: vec![f64::MIN, f64::MAX],
            fields: BTreeMap::from([("count".into(), 10.0)]),
        };
        assert_eq!(encode(&item).unwrap(), serde_json::to_string(&item).unwrap());
    }

    #[test]
    fn nested_nonfinite_numbers_are_rejected() {
        #[derive(Serialize)]
        struct Item {
            fields: BTreeMap<String, Vec<Option<f64>>>,
        }

        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let item = Item {
                fields: BTreeMap::from([("amounts".into(), vec![None, Some(number)])]),
            };
            assert!(matches!(encode(&item), Err(Error::Internal(_))));
        }
        for number in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(encode(&Some(number)), Err(Error::Internal(_))));
        }
    }

    #[test]
    fn every_compound_shape_checks_its_numbers() {
        #[derive(Serialize)]
        struct Newtype(f64);
        #[derive(Serialize)]
        struct Tuple(f64, f64);
        #[derive(Serialize)]
        enum Variant {
            Newtype(f64),
            Tuple(f64, f64),
            Struct { number: f64 },
        }

        assert!(encode(&(0.0, f64::NAN)).is_err());
        assert!(encode(&Newtype(f64::NAN)).is_err());
        assert!(encode(&Tuple(0.0, f64::NAN)).is_err());
        assert!(encode(&Variant::Newtype(f64::NAN)).is_err());
        assert!(encode(&Variant::Tuple(0.0, f64::NAN)).is_err());
        assert!(
            encode(&Variant::Struct {
                number: f64::NAN
            })
            .is_err()
        );
    }

    #[test]
    fn custom_serializers_run_once_and_cannot_emit_nonfinite_numbers() {
        struct Counted<'a> {
            calls: &'a Cell<usize>,
            number: f64,
        }

        impl Serialize for Counted<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                self.calls.set(self.calls.get() + 1);
                serializer.serialize_f64(self.number)
            }
        }

        let calls = Cell::new(0);
        let finite = Counted {
            calls: &calls,
            number: 42.0,
        };
        assert_eq!(encode(&Some(finite)).unwrap(), "42.0");
        assert_eq!(calls.get(), 1);
        let nonfinite = Counted {
            calls: &calls,
            number: f64::NAN,
        };
        assert!(matches!(encode(&vec![nonfinite]), Err(Error::Internal(_))));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn discarding_element_errors_cannot_accept_invalid_numbers_or_oversized_values() {
        struct Discards<T>(T);

        impl<T: Serialize> Serialize for Discards<T> {
            fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(1))?;
                let _ignored = sequence.serialize_element(&self.0);
                sequence.end()
            }
        }

        assert!(matches!(encode(&Discards(f64::NAN)), Err(Error::Internal(_))));
        assert!(matches!(encode(&Discards("x".repeat(MAX_BYTES))), Err(Error::TooLarge)));
    }

    #[test]
    fn custom_serialization_diagnostics_are_redacted() {
        struct Sensitive;

        impl Serialize for Sensitive {
            fn serialize<S: Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
                Err(S::Error::custom("private provider response"))
            }
        }

        assert_eq!(encode(&Sensitive).unwrap_err().to_string(), ENCODING_FAILED);
    }

    /// A JSON string adds two quotes, so exactly `MAX_BYTES` minus two ASCII
    /// bytes fit. The next byte must fail before it is added to the buffer.
    #[test]
    fn item_limit_includes_json_delimiters() {
        let exact = "x".repeat(MAX_BYTES - 2);
        assert_eq!(encode(&exact).unwrap().len(), MAX_BYTES);
        let oversized = "x".repeat(MAX_BYTES - 1);
        assert!(matches!(encode(&oversized), Err(Error::TooLarge)));
    }

    /// NUL expands to six JSON bytes. ASCII padding fills the exact remaining
    /// capacity so escaping cannot conceal an over-limit encoded item.
    #[test]
    fn escaped_output_is_bounded_after_encoding() {
        let exact = format!(
            "{}{}",
            "\0".repeat((MAX_BYTES - 2) / 6),
            "x".repeat((MAX_BYTES - 2) % 6)
        );
        assert_eq!(encode(&exact).unwrap().len(), MAX_BYTES);
        let oversized = format!("{exact}x");
        assert!(matches!(encode(&oversized), Err(Error::TooLarge)));
    }

    #[test]
    fn rejected_write_does_not_grow_output() {
        let mut output = BoundedOutput {
            bytes: Vec::new(),
            exceeded: false,
        };
        output.write_all(&vec![b'x'; MAX_BYTES]).unwrap();
        let capacity = output.bytes.capacity();
        let error = output.write_all(b"x").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::OutOfMemory);
        assert_eq!(output.bytes.len(), MAX_BYTES);
        assert_eq!(output.bytes.capacity(), capacity);
    }
}
