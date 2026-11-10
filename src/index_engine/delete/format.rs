use std::{
    fs,
    io::{Cursor, Read, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::index_engine::types::{DocKey, SeqNo};
use crate::{AppError, Result};

pub(crate) const DELETE_META_FILE: &str = "DELETE_META.json";
pub(crate) const DELETE_RECORDS_FILE: &str = "delete_records.bin";
pub(crate) const DELETE_RECORDS_MAGIC: &[u8; 8] = b"CGDELS01";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRecord {
    pub seq_no: SeqNo,
    pub key: DocKey,
    #[serde(default)]
    pub retract_to_base: bool,
}

pub(crate) fn write_delete_segment_meta(
    path: &Path,
    meta: &crate::index_engine::segment::DeleteSegmentMeta,
) -> Result<()> {
    let encoded = serde_json::to_vec_pretty(meta)
        .map_err(|error| AppError::Protocol(format!("delete meta encode failed: {error}")))?;
    fs::write(path, encoded)?;
    Ok(())
}

pub(crate) fn read_delete_segment_meta(
    path: &Path,
) -> Result<crate::index_engine::segment::DeleteSegmentMeta> {
    serde_json::from_slice::<crate::index_engine::segment::DeleteSegmentMeta>(&fs::read(path)?)
        .map_err(|error| {
            AppError::InvalidIndex(format!(
                "failed to decode delete meta {}: {error}",
                path.display()
            ))
        })
}

pub(crate) fn write_delete_records(path: &Path, records: &[DeleteRecord]) -> Result<()> {
    let mut buffer = Vec::new();
    buffer.write_all(DELETE_RECORDS_MAGIC)?;
    write_u32(&mut buffer, 2)?;
    write_u32(&mut buffer, records.len() as u32)?;
    for record in records {
        write_u64(&mut buffer, record.seq_no)?;
        buffer.push(u8::from(record.retract_to_base));
        write_string(&mut buffer, &record.key.repo_id.0)?;
        write_string(&mut buffer, &record.key.logical_path)?;
    }
    fs::write(path, buffer)?;
    Ok(())
}

pub(crate) fn read_delete_records(path: &Path) -> Result<Vec<DeleteRecord>> {
    let bytes = fs::read(path)?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let mut magic = [0u8; 8];
    cursor.read_exact(&mut magic)?;
    if &magic != DELETE_RECORDS_MAGIC {
        return Err(AppError::InvalidIndex(
            "delete records magic mismatch".into(),
        ));
    }
    let version = read_u32(&mut cursor)?;
    if !matches!(version, 1 | 2) {
        return Err(AppError::InvalidIndex(format!(
            "unsupported delete records version: {version}"
        )));
    }
    let count = read_u32(&mut cursor)? as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let seq_no = read_u64(&mut cursor)?;
        let retract_to_base = if version == 2 {
            let mut flag = [0u8; 1];
            cursor.read_exact(&mut flag)?;
            match flag[0] {
                0 => false,
                1 => true,
                _ => return Err(AppError::InvalidIndex("invalid delete record kind".into())),
            }
        } else {
            false
        };
        let repo_id = crate::index_engine::types::RepoId(read_string(&mut cursor)?);
        let logical_path = read_string(&mut cursor)?;
        records.push(DeleteRecord {
            retract_to_base,
            seq_no,
            key: DocKey {
                repo_id,
                logical_path,
            },
        });
    }
    Ok(records)
}

fn write_u32(buffer: &mut Vec<u8>, value: u32) -> Result<()> {
    buffer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_u64(buffer: &mut Vec<u8>, value: u64) -> Result<()> {
    buffer.write_all(&value.to_le_bytes())?;
    Ok(())
}

fn write_string(buffer: &mut Vec<u8>, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| AppError::ValueOutOfRange("delete string length exceeds u32 range".into()))?;
    write_u32(buffer, len)?;
    buffer.write_all(bytes)?;
    Ok(())
}

fn read_u32(cursor: &mut Cursor<&[u8]>) -> Result<u32> {
    let mut bytes = [0u8; 4];
    cursor.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(cursor: &mut Cursor<&[u8]>) -> Result<u64> {
    let mut bytes = [0u8; 8];
    cursor.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> Result<String> {
    let len = read_u32(cursor)? as usize;
    let mut bytes = vec![0u8; len];
    cursor.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|error| AppError::Protocol(format!("delete string decode failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retraction_records_roundtrip_and_reject_unknown_kind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records");
        let records: Vec<_> = [false, true]
            .into_iter()
            .enumerate()
            .map(|(i, retract_to_base)| DeleteRecord {
                seq_no: i as u64 + 1,
                key: DocKey {
                    repo_id: crate::index_engine::RepoId("repo".into()),
                    logical_path: "file.rs".into(),
                },
                retract_to_base,
            })
            .collect();
        write_delete_records(&path, &records).unwrap();
        assert_eq!(read_delete_records(&path).unwrap(), records);
        let mut bytes = fs::read(&path).unwrap();
        bytes[24] = 2;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            read_delete_records(&path),
            Err(AppError::InvalidIndex(_))
        ));
    }

    #[test]
    fn retraction_decoder_preserves_legacy_deletion_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy");
        let mut bytes = DELETE_RECORDS_MAGIC.to_vec();
        write_u32(&mut bytes, 1).unwrap();
        write_u32(&mut bytes, 1).unwrap();
        write_u64(&mut bytes, 42).unwrap();
        write_string(&mut bytes, "repo").unwrap();
        write_string(&mut bytes, "file.rs").unwrap();
        fs::write(&path, bytes).unwrap();
        let records = read_delete_records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq_no, 42);
        assert_eq!(records[0].key.logical_path, "file.rs");
        assert!(!records[0].retract_to_base);
    }
}
