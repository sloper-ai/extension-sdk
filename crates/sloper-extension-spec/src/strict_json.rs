use std::{
    fmt,
    io::{
        self,
        Write,
    },
};

use serde::Serialize;
use serde_json::{
    Map,
    Value,
};

use super::manifest::{
    MAX_ACTIONS,
    MAX_CONNECTIONS,
    MAX_MANIFEST_BYTES,
    MAX_RESOURCES,
    ManifestError,
};

/// One extension declaration plus the maximum action, resource, and connection
/// entries.
const MAX_PARTS: usize = 1 + MAX_ACTIONS + MAX_RESOURCES + MAX_CONNECTIONS;
/// Maximum encoded object size accepted by the host.
pub(super) const MAX_HOST_OBJECT_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn fits_encoded_limit(value: &impl Serialize, maximum: usize) -> Result<bool, serde_json::Error> {
    struct Counter {
        count: usize,
        maximum: usize,
        exceeded: bool,
    }

    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.count = self.count.saturating_add(bytes.len());
            if self.count > self.maximum {
                self.exceeded = true;
                return Err(io::Error::from(io::ErrorKind::Other));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter {
        count: 0,
        maximum,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        return Ok(false);
    }
    result.map(|()| true)
}
pub(super) fn parse_json(bytes: &[u8]) -> Result<Value, ManifestError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let v = deserialize(&mut de).map_err(ManifestError::json)?;
    de.end().map_err(ManifestError::json)?;
    Ok(v)
}
fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    use serde::de::{
        DeserializeSeed,
        Error,
        MapAccess,
        SeqAccess,
        Visitor,
    };
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("JSON")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(Value::from(v))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(Value::from(v))
        }

        fn visit_f64<E: Error>(self, v: f64) -> Result<Value, E> {
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| E::custom("non-finite"))
        }

        fn visit_str<E>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.into()))
        }

        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }

        fn visit_none<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }

        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut x = Vec::new();
            while let Some(v) = a.next_element_seed(Seed)? {
                x.push(v);
            }
            Ok(Value::Array(x))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut m = Map::new();
            while let Some(k) = a.next_key::<String>()? {
                if m.contains_key(&k) {
                    return Err(Error::custom("duplicate key"));
                }
                m.insert(k, a.next_value_seed(Seed)?);
            }
            Ok(Value::Object(m))
        }
    }
    struct Seed;
    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = Value;

        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            d.deserialize_any(V)
        }
    }
    d.deserialize_any(V)
}

/// Parses bounded, concatenated macro declarations without accepting duplicate
/// keys.
///
/// # Errors
/// Rejects malformed JSON or declarations exceeding manifest bounds.
pub fn parse_fragments(bytes: &[u8]) -> Result<Vec<Value>, ManifestError> {
    struct Fragment(Value);
    impl<'de> serde::Deserialize<'de> for Fragment {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserialize(deserializer).map(Self)
        }
    }
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::invalid(
            "EXTENSION_PARTS_INVALID",
            "",
            "Build declarations exceed 256 KiB.",
        ));
    }
    let mut fragments = Vec::new();
    for fragment in serde_json::Deserializer::from_slice(bytes).into_iter::<Fragment>() {
        if fragments.len() == MAX_PARTS {
            return Err(ManifestError::invalid(
                "EXTENSION_PARTS_INVALID",
                "",
                "Build declaration count exceeds the manifest entry limits.",
            ));
        }
        fragments.push(fragment.map_err(ManifestError::part_json)?.0);
    }
    Ok(fragments)
}

/// Parses a bounded host object without silently accepting duplicate JSON keys.
///
/// # Errors
/// Rejects malformed, non-object, duplicate-key, or oversized operation input.
pub fn parse_object(bytes: &[u8]) -> Result<Value, ManifestError> {
    if bytes.len() > MAX_HOST_OBJECT_BYTES {
        return Err(ManifestError::invalid(
            "MANIFEST_TOO_LARGE",
            "",
            "Host JSON exceeds 8 MiB.",
        ));
    }
    let value = parse_json(bytes)?;
    if !value.is_object() {
        return Err(ManifestError::invalid(
            "MANIFEST_INVALID_VALUE",
            "",
            "Host JSON must be an object.",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde::{
        Serialize,
        Serializer,
        ser::Error as _,
    };

    use super::fits_encoded_limit;

    // The private stop sentinel must not hide unrelated serializer failures.
    #[test]
    fn encoded_limit_counts_exact_bytes_and_propagates_serialization_errors() {
        struct Failing;
        impl Serialize for Failing {
            fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(S::Error::custom("independent failure"))
            }
        }
        assert!(fits_encoded_limit(&"abc", 5).unwrap());
        assert!(!fits_encoded_limit(&"abc", 4).unwrap());
        assert_eq!(
            fits_encoded_limit(&Failing, 100).unwrap_err().to_string(),
            "independent failure"
        );
    }
}
