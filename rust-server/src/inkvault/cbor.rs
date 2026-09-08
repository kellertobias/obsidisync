//! Canonical InkNote CBOR: integer micrometres/time/pressure, string-keyed maps.
use anyhow::{bail, ensure, Result};
use serde_json::{Map, Value};

pub fn decode(bytes: &[u8]) -> Result<Value> {
    ensure!(bytes.len() <= 8 * 1024 * 1024, "InkNote page exceeds 8 MiB");
    let mut decoder = Decoder { bytes, pos: 0 };
    let value = decoder.value(0)?;
    ensure!(decoder.pos == bytes.len(), "trailing CBOR data");
    Ok(value)
}
struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl Decoder<'_> {
    fn byte(&mut self) -> Result<u8> {
        ensure!(self.pos < self.bytes.len(), "truncated CBOR");
        let byte = self.bytes[self.pos];
        self.pos += 1;
        Ok(byte)
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        ensure!(depth <= 32, "CBOR nesting limit");
        let tag = self.byte()?;
        let major = tag >> 5;
        let info = tag & 31;
        if major == 7 {
            return match info {
                20 => Ok(false.into()),
                21 => Ok(true.into()),
                22 => Ok(Value::Null),
                _ => {
                    bail!("InkNote numbers must be integers")
                }
            };
        }
        let n = if info < 24 {
            info as u64
        } else {
            ensure!(
                (24..=27).contains(&info),
                "indefinite CBOR is not canonical"
            );
            let mut n = 0u64;
            for _ in 0..(1 << (info - 24)) {
                n = (n << 8) | self.byte()? as u64;
            }
            let minimum = match info {
                24 => 24,
                25 => 256,
                26 => 65536,
                _ => 0x1_0000_0000,
            };
            ensure!(n >= minimum, "noncanonical CBOR integer/length");
            n
        };
        ensure!(n <= i64::MAX as u64, "integer out of range");
        Ok(match major {
            0 => Value::from(n as i64),
            1 => Value::from(-1 - n as i64),
            3 => {
                ensure!(
                    n <= (self.bytes.len() - self.pos) as u64,
                    "truncated string"
                );
                let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + n as usize])?;
                self.pos += n as usize;
                Value::from(text)
            }
            4 => {
                ensure!(
                    n <= (self.bytes.len() - self.pos) as u64,
                    "invalid array length"
                );
                let mut values = Vec::new();
                for _ in 0..n {
                    values.push(self.value(depth + 1)?);
                }
                Value::Array(values)
            }
            5 => {
                ensure!(
                    n <= ((self.bytes.len() - self.pos) / 2) as u64,
                    "invalid map length"
                );
                let mut values = Map::new();
                let mut previous: Option<Vec<u8>> = None;
                for _ in 0..n {
                    let start = self.pos;
                    let key = self.value(depth + 1)?;
                    let encoded = self.bytes[start..self.pos].to_vec();
                    if let Some(prior) = &previous {
                        ensure!(
                            (prior.len(), prior) < (encoded.len(), &encoded),
                            "noncanonical/duplicate map key"
                        );
                    }
                    previous = Some(encoded);
                    let key = key
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("CBOR map keys must be strings"))?;
                    values.insert(key.into(), self.value(depth + 1)?);
                }
                Value::Object(values)
            }
            _ => bail!("unsupported InkNote CBOR type"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_noncanonical_and_truncated_data() {
        for input in [
            &[0x18, 0x01][..],
            &[0x9a, 0x00, 0x80, 0, 0][..],
            &[1, 2][..],
            &[0xa2, 0x61, b'a', 0, 0x61, b'a', 1][..],
        ] {
            assert!(decode(input).is_err());
        }
        assert_eq!(
            decode(&[0xa1, 0x61, b'a', 0x82, 0x01, 0xf6]).unwrap(),
            serde_json::json!({"a":[1,null]})
        );
    }
}
