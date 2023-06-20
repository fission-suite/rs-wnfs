use num_bigint_dig::BigUint;
use serde::{Deserializer, Serializer};

pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<BigUint, D::Error>
where
    D: Deserializer<'de>,
{
    let bytes: Vec<u8> = serde_bytes::deserialize(deserializer)?;
    Ok(BigUint::from_bytes_le(&bytes))
}

pub(crate) fn serialize<S>(uint: &BigUint, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serde_bytes::serialize(to_bytes_helper(uint).as_ref(), serializer)
}

pub(crate) fn to_bytes_helper(state: &BigUint) -> [u8; 256] {
    let vec = state.to_bytes_le();
    let mut bytes = [0u8; 256];
    bytes[..vec.len()].copy_from_slice(&vec);
    bytes
}
