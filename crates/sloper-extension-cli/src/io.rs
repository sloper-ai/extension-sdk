use std::{
    fs::File,
    io::{
        self,
        Read as _,
    },
    path::Path,
};

use serde::Serialize;
use serde_json::{
    Map,
    Value,
};

use crate::Error;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn text<'a>(arguments: &'a Map<String, Value>, name: &str) -> Result<&'a str, Error> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::usage(format!("{name} is required")))
}

pub(crate) fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::invalid(format!(
            "{} exceeds its {limit}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

pub(crate) fn read_component(path: &Path) -> Result<Vec<u8>, Error> {
    read_bounded(path, MAX_COMPONENT_BYTES)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    append_hex(&mut result, bytes);
    result
}

pub(crate) fn append_hex(result: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
}

pub(crate) fn serialized_exceeds<T: Serialize + ?Sized>(value: &T, limit: usize) -> Result<bool, serde_json::Error> {
    struct Counter {
        remaining: usize,
        exceeded: bool,
    }

    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter {
        remaining: limit,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut counter, value);
    if counter.exceeded {
        Ok(true)
    } else {
        result.map(|()| false)
    }
}
