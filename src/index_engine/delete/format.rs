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
    write_u32(&mut buffer, 1)?;
    write_u32(&mut buffer, records.len() as u32)?;
    for record in records {
        write_u64(&mut buffer, record.seq_no)?;
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
    if version != 1 {
        return Err(AppError::InvalidIndex(format!(
            "unsupported delete records version: {version}"
        )));
    }
    let count = read_u32(&mut cursor)? as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let seq_no = read_u64(&mut cursor)?;
        let repo_id = crate::index_engine::types::RepoId(read_string(&mut cursor)?);
        let logical_path = read_string(&mut cursor)?;
        records.push(DeleteRecord {
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
