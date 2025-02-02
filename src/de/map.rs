//! Serde `Deserializer` module

use crate::{
    de::escape::EscapedDeserializer,
    de::{BorrowingReader, Deserializer, INNER_VALUE, UNFLATTEN_PREFIX},
    errors::serialize::DeError,
    events::{BytesStart, Event},
};
use serde::de::{self, DeserializeSeed, IntoDeserializer};

enum MapValue {
    Empty,
    /// Value should be deserialized from the attribute value
    Attribute {
        value: Vec<u8>,
    },
    Nested,
    /// Value should be deserialized from the text content of the XML node:
    ///
    /// ```xml
    /// <...>text content for field value<...>
    /// ```
    InnerValue,
}

/// A deserializer for `Attributes`
pub(crate) struct MapAccess<'de, 'a, R: BorrowingReader<'de> + 'a> {
    /// Tag -- owner of attributes
    start: BytesStart<'de>,
    de: &'a mut Deserializer<'de, R>,
    /// Position in flat byte slice of all attributes from which next
    /// attribute should be parsed. This field is required because we
    /// do not store reference to `Attributes` itself but instead create
    /// a new object on each advance of `Attributes` iterator, so we need
    /// to restore last position before advance.
    position: usize,
    value: MapValue,
    /// number of fields yet to parse
    size_hint: Option<usize>,
    /// list of fields yet to unflatten (defined as starting with $unflatten=)
    unflatten_fields: Vec<&'static [u8]>,
}

impl<'de, 'a, R: BorrowingReader<'de>> MapAccess<'de, 'a, R> {
    /// Create a new MapAccess
    pub fn new(
        de: &'a mut Deserializer<'de, R>,
        start: BytesStart<'de>,
        fields: &[&'static str],
    ) -> Result<Self, DeError> {
        let position = start.attributes().position;
        Ok(MapAccess {
            de,
            start,
            position,
            value: MapValue::Empty,
            size_hint: if fields.is_empty() {
                None
            } else {
                Some(fields.len())
            },
            unflatten_fields: fields
                .iter()
                .filter(|f| f.starts_with(UNFLATTEN_PREFIX))
                .map(|f| f.as_bytes())
                .collect(),
        })
    }

    fn next_attr(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>, DeError> {
        let mut attributes = self.start.attributes();
        attributes.position = self.position;
        let next_att = attributes.next().transpose()?;
        self.position = attributes.position;
        Ok(next_att.map(|a| (a.key.to_owned(), a.value.into_owned())))
    }
}

impl<'de, 'a, R: BorrowingReader<'de> + 'a> de::MapAccess<'de> for MapAccess<'de, 'a, R> {
    type Error = DeError;

    fn size_hint(&self) -> Option<usize> {
        self.size_hint.clone()
    }

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let decoder = self.de.reader.decoder();
        let has_value_field = self.de.has_value_field;
        if let Some((key, value)) = self.next_attr()? {
            // try getting map from attributes (key= "value")
            self.value = MapValue::Attribute { value };
            self.size_hint.as_mut().map(|l| *l = l.wrapping_sub(1));
            seed.deserialize(EscapedDeserializer::new(key, decoder, false))
                .map(Some)
        } else {
            // try getting from events (<key>value</key>)
            match self.de.peek()? {
                Some(Event::Text(_)) => {
                    self.value = MapValue::InnerValue;
                    // Deserialize `key` from special attribute name which means
                    // that value should be taken from the text content of the
                    // XML node
                    seed.deserialize(INNER_VALUE.into_deserializer()).map(Some)
                }
                // Used to deserialize collections of enums, like:
                // <root>
                //   <A/>
                //   <B/>
                //   <C/>
                // </root>
                //
                // into
                //
                // enum Enum { A, B, С }
                // struct Root {
                //     #[serde(rename = "$value")]
                //     items: Vec<Enum>,
                // }
                // TODO: This should be handled by #[serde(flatten)]
                // See https://github.com/serde-rs/serde/issues/1905
                Some(Event::Start(_)) if has_value_field => {
                    self.value = MapValue::InnerValue;
                    self.size_hint.as_mut().map(|l| *l = l.wrapping_sub(1));
                    seed.deserialize(INNER_VALUE.into_deserializer()).map(Some)
                }
                Some(Event::Start(e)) => {
                    self.size_hint.as_mut().map(|l| *l = l.wrapping_sub(1));
                    let key = if let Some(p) = self
                        .unflatten_fields
                        .iter()
                        .position(|f| e.name() == &f[UNFLATTEN_PREFIX.len()..])
                    {
                        // Used to deserialize elements, like:
                        // <root>
                        //   <xxx>test</xxx>
                        // </root>
                        //
                        // into
                        //
                        // struct Root {
                        //     #[serde(rename = "$unflatten=xxx")]
                        //     xxx: String,
                        // }
                        self.value = MapValue::InnerValue;
                        seed.deserialize(self.unflatten_fields.remove(p).into_deserializer())
                    } else {
                        let name = e.local_name().to_owned();
                        self.value = MapValue::Nested;
                        seed.deserialize(EscapedDeserializer::new(name, decoder, false))
                    };
                    key.map(Some)
                }
                _ => Ok(None),
            }
        }
    }

    fn next_value_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<K::Value, Self::Error> {
        match std::mem::replace(&mut self.value, MapValue::Empty) {
            MapValue::Attribute { value } => seed.deserialize(EscapedDeserializer::new(
                value,
                self.de.reader.decoder(),
                true,
            )),
            MapValue::Nested | MapValue::InnerValue => seed.deserialize(&mut *self.de),
            MapValue::Empty => Err(DeError::EndOfAttributes),
        }
    }
}
