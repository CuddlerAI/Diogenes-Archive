//! Lossless, bounded draw-field interning. Runs on the existing writer, never in a hook.
//! Raw JSON tokens (including f64 and u64) are copied, not parsed and reserialized.
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::{HashMap, HashSet};

pub const REPORT_SCHEMA: &str = "dm-cp2077-render-camera-observation/v3";
pub const ARCHIVE_SCHEMA: &str = "dm-cp2077-camera-archive/v2";
pub const BLOCK_SCHEMA: &str = "dm-cp2077-camera-draw-block/v1";
pub const BLOCK_ROWS: usize = 64;
pub const BLOCK_EXPANDED_BYTES: usize = 4 * 1024 * 1024;
pub const BLOCK_BYTES: u64 = 8 * 1024 * 1024;
const FIELDS: usize = 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    schema: String,
    keys: Vec<String>,
    values: Vec<Box<RawValue>>,
    rows: Vec<Vec<[u32; 2]>>,
    expanded_bytes: usize,
}

// A map visitor retains field order and each original value's exact token.
struct Fields(Vec<(String, Box<RawValue>)>);
impl<'de> Deserialize<'de> for Fields {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Fields;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a bounded draw object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Fields, M::Error> {
                let mut fields = Vec::new();
                while let Some(pair) = map.next_entry()? {
                    if fields.len() == FIELDS {
                        return Err(serde::de::Error::custom("too many draw fields"));
                    }
                    fields.push(pair);
                }
                Ok(Fields(fields))
            }
        }
        de.deserialize_map(Visitor)
    }
}

impl Block {
    pub fn encode(rows: &[&RawValue]) -> Result<Self, String> {
        if rows.is_empty() || rows.len() > BLOCK_ROWS {
            return Err("camera block row count exceeds bound".into());
        }
        let mut block = Self {
            schema: BLOCK_SCHEMA.into(),
            keys: Vec::new(),
            values: Vec::new(),
            rows: Vec::new(),
            expanded_bytes: 0,
        };
        let mut keys = HashMap::new();
        let mut values = HashMap::new();
        for row in rows {
            if row.get().len() as u64 + 1 > super::ARCHIVE_ROW_BYTES {
                return Err("camera draw exceeds row bound".into());
            }
            block.expanded_bytes += row.get().len() + 1;
            if block.expanded_bytes > BLOCK_EXPANDED_BYTES {
                return Err("camera block expansion exceeds bound".into());
            }
            let fields: Fields = serde_json::from_str(row.get()).map_err(|e| e.to_string())?;
            let mut refs = Vec::new();
            for (key, value) in fields.0 {
                let next = keys.len() as u32;
                let ki = *keys.entry(key.clone()).or_insert_with(|| {
                    block.keys.push(key);
                    next
                });
                let next = values.len() as u32;
                let vi = *values.entry(value.get().to_owned()).or_insert_with(|| {
                    block.values.push(value);
                    next
                });
                refs.push([ki, vi]);
            }
            block.rows.push(refs);
        }
        // Reject noncanonical spacing/escaped keys or duplicate fields instead of silently
        // changing original bytes. All native draw serializers emit this compact form.
        let restored = block.decode()?;
        if restored.iter().zip(rows).any(|(a, b)| a.get() != b.get()) {
            return Err("camera block would change original JSON tokens".into());
        }
        Ok(block)
    }

    pub fn decode(&self) -> Result<Vec<Box<RawValue>>, String> {
        if self.schema != BLOCK_SCHEMA
            || self.rows.is_empty()
            || self.rows.len() > BLOCK_ROWS
            || self.expanded_bytes > BLOCK_EXPANDED_BYTES
            || self.keys.len() > BLOCK_ROWS * FIELDS
            || self.values.len() > BLOCK_ROWS * FIELDS
        {
            return Err("invalid camera block schema/bounds".into());
        }
        let keys: Vec<_> = self
            .keys
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if self.keys.iter().collect::<HashSet<_>>().len() != self.keys.len() {
            return Err("duplicate camera block key".into());
        }
        let mut used_keys = HashSet::new();
        let mut used_values = HashSet::new();
        let mut output = Vec::new();
        let mut total = 0;
        for row in &self.rows {
            if row.is_empty() || row.len() > FIELDS {
                return Err("invalid camera draw fields".into());
            }
            let mut seen = HashSet::new();
            let mut text = String::from("{");
            for &[ki, vi] in row {
                let key = keys
                    .get(ki as usize)
                    .ok_or("camera block key reference out of range")?;
                let value = self
                    .values
                    .get(vi as usize)
                    .ok_or("camera block value reference out of range")?;
                if !seen.insert(ki) {
                    return Err("duplicate field in camera draw".into());
                }
                used_keys.insert(ki);
                used_values.insert(vi);
                let extra = usize::from(text.len() > 1) + key.len() + 1 + value.get().len();
                if text.len() + extra + 2 > super::ARCHIVE_ROW_BYTES as usize
                    || total + text.len() + extra + 2 > BLOCK_EXPANDED_BYTES
                {
                    return Err("camera block expansion exceeds bound".into());
                }
                if text.len() > 1 {
                    text.push(',');
                }
                text.push_str(key);
                text.push(':');
                text.push_str(value.get());
            }
            text.push('}');
            total += text.len() + 1;
            output.push(RawValue::from_string(text).map_err(|e| e.to_string())?);
        }
        if total != self.expanded_bytes
            || used_keys.len() != self.keys.len()
            || used_values.len() != self.values.len()
        {
            return Err("camera block byte count or unused dictionary entry".into());
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_retains_order_optional_fields_and_numeric_tokens() {
        let a = RawValue::from_string(r#"{"seq":18446744073709551615,"m":[-0.0,5e-324,-1.0000003576278687],"source":"same","optional":null}"#.into()).unwrap();
        let b = RawValue::from_string(
            r#"{"m":[-0.0,5e-324,-1.0000003576278687],"source":"same","seq":18446744073709551614}"#
                .into(),
        )
        .unwrap();
        let block = Block::encode(&[&a, &b]).unwrap();
        assert_eq!(block.values.len(), 5);
        let result = block.decode().unwrap();
        assert_eq!(result[0].get(), a.get());
        assert_eq!(result[1].get(), b.get());
    }
    #[test]
    fn rejects_bad_references_duplicate_keys_expansion_and_schema() {
        let row = RawValue::from_string(r#"{"a":1,"b":2}"#.into()).unwrap();
        let good = serde_json::to_value(Block::encode(&[&row]).unwrap()).unwrap();
        for (pointer, replacement) in [
            ("/rows/0/0/1", serde_json::json!(99)),
            ("/rows/0/1/0", serde_json::json!(0)),
            ("/expanded_bytes", serde_json::json!(0)),
            ("/schema", serde_json::json!("future")),
        ] {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                serde_json::from_value::<Block>(bad)
                    .unwrap()
                    .decode()
                    .is_err()
            );
        }
        let mut bomb = Block::encode(&[&row]).unwrap();
        bomb.values[0] =
            RawValue::from_string(format!("\"{}\"", "x".repeat(BLOCK_EXPANDED_BYTES))).unwrap();
        assert!(bomb.decode().is_err());
        let duplicate = RawValue::from_string(r#"{"a":1,"a":2}"#.into()).unwrap();
        assert!(Block::encode(&[&duplicate]).is_err());
    }
}
