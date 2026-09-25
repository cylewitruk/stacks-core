//! Consensus serialization of retained values without constructing an owned value tree.

use std::io::Write;

use super::{
    PackedPrincipalView, PackedValueError, PackedValueKind, PackedValueView, SharedPackedValue,
};
use crate::types::serialization::{SerializationError, TypePrefix};

/// Convert an impossible admitted-view failure into the serializer's error channel.
fn view_error(error: PackedValueError) -> SerializationError {
    SerializationError::SerializationFailure(error.to_string())
}

impl PackedValueView<'_> {
    /// Stream canonical consensus bytes directly from owned or mapped child payloads.
    pub fn serialize_write<W: Write>(self, writer: &mut W) -> Result<(), SerializationError> {
        match self.kind().map_err(view_error)? {
            PackedValueKind::Int => {
                writer.write_all(&[TypePrefix::Int as u8])?;
                writer.write_all(&self.as_int().expect("integer kind").to_be_bytes())?;
            }
            PackedValueKind::UInt => {
                writer.write_all(&[TypePrefix::UInt as u8])?;
                writer.write_all(&self.as_uint().expect("unsigned kind").to_be_bytes())?;
            }
            PackedValueKind::Bool => {
                writer.write_all(&[if self.as_bool().expect("boolean kind") {
                    TypePrefix::BoolTrue
                } else {
                    TypePrefix::BoolFalse
                } as u8])?
            }
            PackedValueKind::Principal | PackedValueKind::Callable => {
                match self
                    .as_principal()
                    .map_err(view_error)?
                    .expect("principal kind")
                {
                    PackedPrincipalView::Standard { version, hash } => {
                        writer.write_all(&[TypePrefix::PrincipalStandard as u8, version])?;
                        writer.write_all(hash)?;
                    }
                    PackedPrincipalView::Contract {
                        issuer_version,
                        issuer_hash,
                        name,
                    } => {
                        writer.write_all(&[TypePrefix::PrincipalContract as u8, issuer_version])?;
                        writer.write_all(issuer_hash)?;
                        writer.write_all(&[name.len() as u8])?;
                        writer.write_all(name.as_bytes())?;
                    }
                }
            }
            PackedValueKind::Buffer | PackedValueKind::Ascii | PackedValueKind::Utf8 => {
                let kind = self.kind().map_err(view_error)?;
                let prefix = match kind {
                    PackedValueKind::Buffer => TypePrefix::Buffer,
                    PackedValueKind::Ascii => TypePrefix::StringASCII,
                    _ => TypePrefix::StringUTF8,
                };
                let len = self.sequence_byte_len().expect("byte sequence kind");
                writer.write_all(&[prefix as u8])?;
                writer.write_all(&(len as u32).to_be_bytes())?;
                self.write_sequence_bytes(writer)?;
            }
            PackedValueKind::Optional => match self.optional_child().map_err(view_error)? {
                None => writer.write_all(&[TypePrefix::OptionalNone as u8])?,
                Some(child) => {
                    writer.write_all(&[TypePrefix::OptionalSome as u8])?;
                    child.serialize_write(writer)?;
                }
            },
            PackedValueKind::Response => {
                let (committed, child) = self.response_child().map_err(view_error)?;
                writer.write_all(&[if committed {
                    TypePrefix::ResponseOk
                } else {
                    TypePrefix::ResponseErr
                } as u8])?;
                child.serialize_write(writer)?;
            }
            PackedValueKind::List => {
                let list = self.as_list().map_err(view_error)?;
                writer.write_all(&[TypePrefix::List as u8])?;
                writer.write_all(&(list.len() as u32).to_be_bytes())?;
                for i in 0..list.len() {
                    list.get(i)
                        .map_err(view_error)?
                        .expect("list index")
                        .serialize_write(writer)?;
                }
            }
            PackedValueKind::Tuple => {
                let tuple = self.as_tuple().map_err(view_error)?;
                writer.write_all(&[TypePrefix::Tuple as u8])?;
                writer.write_all(&(tuple.len() as u32).to_be_bytes())?;
                for i in 0..tuple.len() {
                    let (name, child) = tuple
                        .get_index(i)
                        .map_err(view_error)?
                        .expect("tuple index");
                    writer.write_all(&[name.len() as u8])?;
                    writer.write_all(name.as_bytes())?;
                    child.serialize_write(writer)?;
                }
            }
        }
        Ok(())
    }
}

impl SharedPackedValue {
    /// Serialize any retained representation without recursive owned materialization.
    pub fn serialize_write<W: Write>(&self, writer: &mut W) -> Result<(), SerializationError> {
        self.as_view().serialize_write(writer)
    }

    /// Allocate only the final consensus output buffer.
    pub fn serialize_to_vec(&self) -> Result<Vec<u8>, SerializationError> {
        let mut bytes = Vec::new();
        self.serialize_write(&mut bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use stacks_common::types::StacksEpochId;

    use super::*;
    use crate::types::{PrincipalData, QualifiedContractIdentifier, TupleData, Value};

    /// Every active value family emits the same consensus bytes without an owned intermediate.
    #[test]
    fn streaming_serialization_matches_all_owned_families() {
        let epoch = StacksEpochId::latest();
        let values = vec![
            Value::Int(-123),
            Value::UInt(u128::MAX),
            Value::Bool(false),
            Value::Bool(true),
            Value::none(),
            Value::some(Value::UInt(7)).unwrap(),
            Value::okay(Value::UInt(3)).unwrap(),
            Value::error(Value::Int(-4)).unwrap(),
            Value::Principal(PrincipalData::Contract(
                QualifiedContractIdentifier::local("sample").unwrap(),
            )),
            Value::buff_from(vec![0, 128, 255]).unwrap(),
            Value::string_ascii_from_bytes(b"abc".to_vec()).unwrap(),
            Value::string_utf8_from_bytes("aé😀".as_bytes().to_vec()).unwrap(),
            Value::cons_list(vec![Value::UInt(1), Value::UInt(2)], &epoch).unwrap(),
            Value::Tuple(
                TupleData::from_data(vec![(
                    "item".to_string().try_into().unwrap(),
                    Value::Bool(true),
                )])
                .unwrap(),
            ),
        ];
        for value in values {
            let expected = value.serialize_to_vec().unwrap();
            let shared = SharedPackedValue::from_value(value, &epoch).unwrap();
            SharedPackedValue::reset_materialization_count();
            assert_eq!(shared.serialize_to_vec().unwrap(), expected);
            assert_eq!(SharedPackedValue::materialization_count(), 0);
            assert!(shared.serialize_write(&mut BrokenWriter).is_err());
        }
    }

    /// Fail every output operation to verify that serialization propagates sink errors.
    struct BrokenWriter;
    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("test sink"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
