use std::{borrow::Cow, fmt};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use zeroize::Zeroize;

pub(crate) enum LosslessJsonValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<LosslessJsonValue>),
    Object(Vec<(String, LosslessJsonValue)>),
}

impl LosslessJsonValue {
    pub(crate) fn last_field(&self, name: &str) -> Option<&Self> {
        let Self::Object(fields) = self else {
            return None;
        };
        fields
            .iter()
            .rev()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }

    pub(crate) fn scalar_text(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::String(value) => Some(Cow::Borrowed(value)),
            Self::Number(value) => Some(Cow::Owned(value.to_string())),
            Self::Null | Self::Bool(_) | Self::Array(_) | Self::Object(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for LosslessJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(LosslessJsonValueVisitor)
    }
}

struct LosslessJsonValueVisitor;

impl<'de> Visitor<'de> for LosslessJsonValueVisitor {
    type Value = LosslessJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(LosslessJsonValue::Number)
            .ok_or_else(|| E::custom("JSON number must be finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(LosslessJsonValue::String(value))
    }

    fn visit_seq<A>(self, mut values: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut sequence = Vec::new();
        while let Some(value) = values.next_element()? {
            sequence.push(value);
        }
        Ok(LosslessJsonValue::Array(sequence))
    }

    fn visit_map<A>(self, mut fields: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Vec::new();
        while let Some((field, value)) = fields.next_entry()? {
            object.push((field, value));
        }
        Ok(LosslessJsonValue::Object(object))
    }
}

impl Zeroize for LosslessJsonValue {
    fn zeroize(&mut self) {
        match self {
            Self::Null => {}
            Self::Bool(value) => value.zeroize(),
            Self::Number(value) => *value = serde_json::Number::from(0),
            Self::String(value) => value.zeroize(),
            Self::Array(values) => values.zeroize(),
            Self::Object(fields) => fields.zeroize(),
        }
    }
}

impl Drop for LosslessJsonValue {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl fmt::Debug for LosslessJsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("LosslessJsonValue::Null"),
            Self::Bool(_) => formatter.write_str("LosslessJsonValue::Bool"),
            Self::Number(_) => formatter.write_str("LosslessJsonValue::Number"),
            Self::String(_) => formatter.write_str("LosslessJsonValue::String"),
            Self::Array(values) => formatter
                .debug_struct("LosslessJsonValue::Array")
                .field("len", &values.len())
                .finish(),
            Self::Object(fields) => formatter
                .debug_struct("LosslessJsonValue::Object")
                .field("len", &fields.len())
                .finish(),
        }
    }
}
