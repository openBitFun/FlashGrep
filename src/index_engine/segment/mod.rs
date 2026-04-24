mod format;
mod live_docs;
mod mmap;
mod path_map;
mod postings;
mod reader;
mod term_dict;
mod writer;

pub(crate) use format::{
    file_name_from_logical_path, language_from_logical_path, read_encoded_source_bytes_view,
    read_index_document_from_inline_slice, read_segment_meta, write_path_offsets_file,
    write_segment_documents, write_segment_meta, FileSegmentDocTable, SegmentDocRecord,
    SegmentDocSourceKind, DOCS_FILE, PATH_MAP_FILE, PATH_OFFSETS_FILE, SEGMENT_META_FILE,
};
pub use format::{DeleteSegmentBuildOutput, DeleteSegmentMeta, SegmentBuildOutput, SegmentMeta};
pub(crate) use mmap::{advise_mmap_random, advise_mmap_sequential};
pub(crate) use postings::encoded_posting_len;
#[cfg(test)]
pub(crate) use postings::read_postings_file;
pub(crate) use postings::{write_postings_file, PostingLists, PostingsStreamWriter};
pub(crate) use reader::{SegmentReaderHandle, SegmentVerifySource};
#[cfg(test)]
pub(crate) use term_dict::read_term_dict_file;
pub(crate) use term_dict::{
    build_posting_lists, collect_document_posting_terms, collect_document_posting_terms_from_text,
    collect_query_posting_terms, ordered_posting_keys, write_term_dict_file, TermDictEntry,
    TermDictStreamWriter, TermDictionary,
};
pub(crate) use writer::{FileSegmentStore, SegmentStore};
