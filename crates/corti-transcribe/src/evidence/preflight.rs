//! Readback's first pass never owns transcript text or a collection. serde_json may allocate one
//! reusable unescape buffer, bounded by twice the input length. Only after these checks may the
//! typed second pass allocate owners. It also rejects unknown array shapes before any Vec exists.
//! The retained bound is separate from the conservative parsing
//! envelope: 16 * owned_limit + 4 * wire_limit, including enum buffering, vector growth, validation
//! and compaction. Neither bound includes the caller's input slice, allocator overhead, or RSS.
use super::*;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::fmt;

pub(super) fn check(bytes: &[u8], owned_cap: usize) -> Result<(), Error> {
    let mut budget = Budget {
        owned: std::mem::size_of::<Ledger>() + QUALITY_OWNED_RESERVE,
        owned_cap,
        quality: 0,
        nodes: 0,
        strings: 0,
        transient_cap: owned_cap
            .checked_mul(16)
            .and_then(|n| {
                bytes
                    .len()
                    .checked_mul(4)
                    .and_then(|wire| n.checked_add(wire))
            })
            .ok_or(Error::Capacity)?,
        capacity: false,
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let result = Node {
        budget: &mut budget,
        key: Key::Other,
        quality: false,
        buffered: false,
    }
    .deserialize(&mut decoder);
    if result.is_err() {
        return Err(if budget.capacity {
            Error::Capacity
        } else {
            Error::InvalidDocument
        });
    }
    decoder.end().map_err(|_| Error::InvalidDocument)
}

struct Budget {
    owned: usize,
    owned_cap: usize,
    quality: usize,
    nodes: usize,
    strings: usize,
    transient_cap: usize,
    capacity: bool,
}
impl Budget {
    fn add<E: de::Error>(&mut self, bytes: usize, quality: bool) -> Result<(), E> {
        let (counter, limit) = if quality {
            (&mut self.quality, QUALITY_OWNED_RESERVE)
        } else {
            (&mut self.owned, self.owned_cap)
        };
        *counter = counter.saturating_add(bytes);
        if *counter > limit {
            self.capacity = true;
            return Err(E::custom("evidence capacity"));
        }
        self.bound()
    }
    fn node<E: de::Error>(&mut self, buffered: bool) -> Result<(), E> {
        self.nodes = self.nodes.saturating_add(usize::from(buffered));
        self.bound()
    }
    fn bound<E: de::Error>(&mut self) -> Result<(), E> {
        // Only internally tagged enum subtrees buffer serde Content nodes. Plain typed structs,
        // row IDs and scalar fields do not allocate one node each. Charging them as if they did
        // rejects valid dense documents at their identical owned policy. Count the actual buffered
        // subtrees conservatively (128 bytes/node); four owner copies cover Vec growth, compaction
        // and validation indexes. Unescape scratch is covered by the string/wire allowance.
        let predicted = self
            .nodes
            .saturating_mul(128)
            .saturating_add(self.strings.saturating_mul(4))
            .saturating_add(self.owned.saturating_add(self.quality).saturating_mul(4));
        if predicted > self.transient_cap {
            self.capacity = true;
            return Err(E::custom("evidence capacity"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Key {
    Other,
    Buffered,
    Text,
    Groups,
    Rows,
    Stages,
    Decisions,
    Derivations,
    Edits,
    RowIds,
    Parents,
    Indices,
    Epochs,
    Spans,
    Quality,
    Counts,
}
impl Key {
    fn read(value: &str) -> Self {
        match value {
            "kind" | "speaker" | "action" => Self::Buffered,
            "text" | "label" | "configuration_hash" | "model_hash" | "runtime_hash" | "rule_id" => {
                Self::Text
            }
            "groups" => Self::Groups,
            "rows" => Self::Rows,
            "stages" => Self::Stages,
            "decisions" => Self::Decisions,
            "derivations" => Self::Derivations,
            "edits" => Self::Edits,
            "output" | "originals" | "replacements" | "protected_output" => Self::RowIds,
            "assembled_input" | "context" => Self::Parents,
            "input_indices" => Self::Indices,
            "epochs" => Self::Epochs,
            "spans" => Self::Spans,
            "final_quality" => Self::Quality,
            "omitted_spans" | "outstanding_omitted_spans" | "identity" => Self::Counts,
            _ => Self::Other,
        }
    }
    fn element_bytes(self) -> Option<usize> {
        Some(match self {
            Self::Groups => std::mem::size_of::<Group>(),
            Self::Rows => std::mem::size_of::<Row>(),
            Self::Stages => std::mem::size_of::<Stage>(),
            Self::Decisions => std::mem::size_of::<Decision>(),
            Self::Derivations => std::mem::size_of::<Derivation>(),
            Self::Edits => std::mem::size_of::<TextEdit>(),
            Self::RowIds => std::mem::size_of::<RowId>(),
            Self::Parents => std::mem::size_of::<ParentRow>(),
            Self::Indices => std::mem::size_of::<u64>(),
            Self::Epochs => std::mem::size_of::<corti_core::CaptureEpoch>(),
            Self::Spans => std::mem::size_of::<corti_core::QualitySpan>(),
            Self::Counts => 0,
            _ => return None,
        })
    }
}
struct KeySeed;
impl<'de> DeserializeSeed<'de> for KeySeed {
    type Value = Key;
    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<Key, D::Error> {
        decoder.deserialize_str(self)
    }
}
impl Visitor<'_> for KeySeed {
    type Value = Key;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("evidence field")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Key, E> {
        Ok(Key::read(value))
    }
}
struct Node<'a> {
    budget: &'a mut Budget,
    key: Key,
    quality: bool,
    buffered: bool,
}
impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        self.budget.node(self.buffered)?;
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Node<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded evidence")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        self.budget.strings = self.budget.strings.saturating_add(value.len());
        if matches!(self.key, Key::Text) {
            self.budget.add(value.len(), self.quality)?;
        }
        self.budget.node(self.buffered)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key_seed(KeySeed)? {
            map.next_value_seed(Node {
                budget: self.budget,
                key,
                quality: self.quality || matches!(key, Key::Quality),
                buffered: self.buffered || matches!(key, Key::Buffered),
            })?;
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let element = self
            .key
            .element_bytes()
            .ok_or_else(|| de::Error::custom("evidence shape"))?;
        while seq
            .next_element_seed(Node {
                budget: self.budget,
                key: Key::Other,
                quality: self.quality,
                buffered: self.buffered,
            })?
            .is_some()
        {
            self.budget.add(element, self.quality)?;
        }
        Ok(())
    }
}
