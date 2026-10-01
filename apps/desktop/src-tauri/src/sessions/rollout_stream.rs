//! Stream JSONL through private disk snapshots. No conversation record or
//! unknown JSON string is materialized in memory, including image/tool data.

use crate::error::{CodexxError, Result};
use crate::file_io::io_err;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tempfile::TempDir;

const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_JSON_DEPTH: usize = 128;
const MAX_KEY_CAPTURE_BYTES: usize = 128;
const MAX_METADATA_STRING_BYTES: usize = 4096;
const MAX_CWD_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub(super) struct FirstMeta {
    pub(super) id: String,
    pub(super) cwd: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) is_internal: bool,
}

#[derive(Debug)]
pub(super) struct StreamedSnapshot {
    // The Arc owns the directory until the last transaction/rollback user ends.
    _directory: TempDir,
    pub(super) original_path: PathBuf,
    pub(super) next_path: PathBuf,
    pub(super) original_hash: [u8; 32],
    pub(super) next_hash: [u8; 32],
}

#[derive(Debug)]
pub(super) struct StreamedRollout {
    pub(super) first_meta: FirstMeta,
    pub(super) session_meta_count: usize,
    pub(super) mismatch_count: usize,
    pub(super) original_hash: [u8; 32],
    pub(super) original_mtime: Option<SystemTime>,
    pub(super) staged: Option<Arc<StreamedSnapshot>>,
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| io_err(path, error))
}

fn private_directory(prefix: &str, path: &Path) -> Result<TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    builder.tempdir().map_err(|error| io_err(path, error))
}

/// Frame arbitrary-size records into one reusable private disk spool. The
/// callback receives the original bytes, including CRLF or a missing last LF.
/// Callbacks may read/seek the spool but must not modify its bytes or length.
pub(super) fn for_each_jsonl_record<R, F>(reader: R, visitor: F) -> Result<()>
where
    R: Read,
    F: FnMut(&mut File, u64) -> Result<()>,
{
    let directory = private_directory("codex-x-jsonl-record-", Path::new("JSONL spool"))?;
    frame_records(reader, directory.path(), Path::new("JSONL source"), visitor)
}

fn frame_records<R, F>(reader: R, directory: &Path, source: &Path, mut visitor: F) -> Result<()>
where
    R: Read,
    F: FnMut(&mut File, u64) -> Result<()>,
{
    let spool_path = directory.join("record.jsonl");
    let mut spool = private_file(&spool_path)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, reader);
    let mut record_bytes = 0u64;
    let mut spool_len = 0u64;
    loop {
        let buffer = reader.fill_buf().map_err(|error| io_err(source, error))?;
        if buffer.is_empty() {
            if record_bytes != 0 {
                if record_bytes < spool_len {
                    spool
                        .set_len(record_bytes)
                        .map_err(|error| io_err(&spool_path, error))?;
                }
                spool
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| io_err(&spool_path, error))?;
                visitor(&mut spool, record_bytes)?;
            }
            break;
        }
        let ending = buffer.iter().position(|byte| *byte == b'\n');
        let count = ending.map_or(buffer.len(), |index| index + 1);
        spool
            .write_all(&buffer[..count])
            .map_err(|error| io_err(&spool_path, error))?;
        record_bytes = record_bytes
            .checked_add(count as u64)
            .ok_or_else(|| CodexxError::Config("JSONL record size overflow".into()))?;
        spool_len = spool_len.max(record_bytes);
        reader.consume(count);
        if ending.is_some() {
            if record_bytes < spool_len {
                spool
                    .set_len(record_bytes)
                    .map_err(|error| io_err(&spool_path, error))?;
                spool_len = record_bytes;
            }
            spool
                .seek(SeekFrom::Start(0))
                .map_err(|error| io_err(&spool_path, error))?;
            visitor(&mut spool, record_bytes)?;
            spool
                .seek(SeekFrom::Start(0))
                .map_err(|error| io_err(&spool_path, error))?;
            record_bytes = 0;
        }
    }
    // Do not retain the final (potentially huge) record beside the snapshots.
    drop(spool);
    fs::remove_file(&spool_path).map_err(|error| io_err(&spool_path, error))?;
    Ok(())
}

struct RawCapture {
    bytes: Option<Vec<u8>>,
    limit: usize,
    required: bool,
}

impl RawCapture {
    fn ignored() -> Self {
        Self {
            bytes: None,
            limit: 0,
            required: false,
        }
    }
    fn limited(limit: usize, required: bool) -> Self {
        Self {
            bytes: Some(Vec::new()),
            limit,
            required,
        }
    }
    fn append(&mut self, bytes: &[u8], path: &Path) -> Result<()> {
        if let Some(capture) = &mut self.bytes {
            if bytes.len() > self.limit.saturating_sub(capture.len()) {
                if self.required {
                    return Err(format_error(path, "必要的元数据字符串过长"));
                }
                self.bytes = None;
            } else {
                capture.extend_from_slice(bytes);
            }
        }
        Ok(())
    }
}

fn format_error(path: &Path, reason: &str) -> CodexxError {
    CodexxError::Config(format!(
        "会话 JSONL 格式无效（{reason}）: {}",
        path.display()
    ))
}

/// A byte-level validator avoids serde_json's scratch allocation for very
/// large object keys or recognized string values. Only short selected strings
/// are decoded with serde_json; everything else is validated and streamed past.
struct JsonReader<'a, R> {
    input: BufReader<R>,
    path: &'a Path,
    offset: u64,
}

impl<'a, R: Read> JsonReader<'a, R> {
    fn new(reader: R, path: &'a Path) -> Self {
        Self {
            input: BufReader::with_capacity(IO_BUFFER_BYTES, reader),
            path,
            offset: 0,
        }
    }
    fn peek(&mut self) -> Result<Option<u8>> {
        Ok(self
            .input
            .fill_buf()
            .map_err(|error| io_err(self.path, error))?
            .first()
            .copied())
    }
    fn byte(&mut self) -> Result<u8> {
        let byte = self
            .peek()?
            .ok_or_else(|| format_error(self.path, "记录未结束"))?;
        self.input.consume(1);
        self.offset += 1;
        Ok(byte)
    }
    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.byte()? != byte {
            return Err(format_error(self.path, "JSON 分隔符无效"));
        }
        Ok(())
    }
    fn whitespace(&mut self) -> Result<()> {
        loop {
            let buffer = self
                .input
                .fill_buf()
                .map_err(|error| io_err(self.path, error))?;
            let count = buffer
                .iter()
                .take_while(|byte| matches!(**byte, b' ' | b'\t' | b'\r' | b'\n'))
                .count();
            if count == 0 {
                return Ok(());
            }
            self.input.consume(count);
            self.offset += count as u64;
        }
    }
    fn end(&mut self) -> Result<()> {
        self.whitespace()?;
        if self.peek()?.is_some() {
            return Err(format_error(self.path, "一行包含多条 JSON 或尾随数据"));
        }
        Ok(())
    }
    fn hex_quad(&mut self, raw: &mut RawCapture) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let byte = self.byte()?;
            raw.append(&[byte], self.path)?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(format_error(self.path, "Unicode 转义无效")),
            };
            value = (value << 4) | digit as u16;
        }
        Ok(value)
    }
    fn string(&mut self, mut raw: RawCapture) -> Result<Option<String>> {
        self.expect(b'"')?;
        raw.append(b"\"", self.path)?;
        loop {
            // Most image/tool strings are ASCII. Consume entire fixed-size
            // chunks rather than dispatching one reader operation per byte.
            let buffer = self
                .input
                .fill_buf()
                .map_err(|error| io_err(self.path, error))?;
            let count = buffer
                .iter()
                .take_while(|byte| {
                    **byte >= 0x20 && **byte < 0x80 && !matches!(**byte, b'"' | b'\\')
                })
                .count();
            if count != 0 {
                raw.append(&buffer[..count], self.path)?;
                self.input.consume(count);
                self.offset += count as u64;
                continue;
            }
            let byte = self.byte()?;
            raw.append(&[byte], self.path)?;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = self.byte()?;
                    raw.append(&[escape], self.path)?;
                    match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            let codepoint = self.hex_quad(&mut raw)?;
                            if (0xd800..=0xdbff).contains(&codepoint) {
                                self.expect(b'\\')?;
                                self.expect(b'u')?;
                                raw.append(b"\\u", self.path)?;
                                if !(0xdc00..=0xdfff).contains(&self.hex_quad(&mut raw)?) {
                                    return Err(format_error(self.path, "Unicode 代理对无效"));
                                }
                            } else if (0xdc00..=0xdfff).contains(&codepoint) {
                                return Err(format_error(self.path, "孤立的 Unicode 代理值"));
                            }
                        }
                        _ => return Err(format_error(self.path, "字符串转义无效")),
                    }
                }
                0..=0x1f => return Err(format_error(self.path, "字符串含未转义控制字符")),
                0x80..=0xff => {
                    let width = match byte {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(format_error(self.path, "UTF-8 无效")),
                    };
                    let mut encoded = [0u8; 4];
                    encoded[0] = byte;
                    for next in encoded.iter_mut().take(width).skip(1) {
                        *next = self.byte()?;
                        raw.append(&[*next], self.path)?;
                    }
                    std::str::from_utf8(&encoded[..width])
                        .map_err(|_| format_error(self.path, "UTF-8 无效"))?;
                }
                _ => return Err(format_error(self.path, "字符串未结束")),
            }
        }
        raw.bytes
            .map(|bytes| {
                serde_json::from_slice::<String>(&bytes)
                    .map_err(|_| format_error(self.path, "字符串解码失败"))
            })
            .transpose()
    }
    fn key(&mut self) -> Result<Option<String>> {
        self.string(RawCapture::limited(MAX_KEY_CAPTURE_BYTES, false))
    }
    fn short_string(&mut self, limit: usize, required: bool, depth: usize) -> Result<ShortString> {
        self.whitespace()?;
        if self.peek()? != Some(b'"') {
            self.value(depth)?;
            return Ok(ShortString {
                is_string: false,
                value: None,
            });
        }
        let value = self.string(RawCapture::limited(
            limit.saturating_mul(6).saturating_add(2),
            required,
        ))?;
        let value = match value {
            Some(value) if value.len() <= limit => Some(value),
            Some(_) if required => return Err(format_error(self.path, "必要的元数据字符串过长")),
            _ => None,
        };
        Ok(ShortString {
            is_string: true,
            value,
        })
    }
    fn literal(&mut self, expected: &[u8]) -> Result<()> {
        for byte in expected {
            self.expect(*byte)?;
        }
        Ok(())
    }
    fn digits(&mut self) -> Result<usize> {
        let mut total = 0;
        loop {
            let buffer = self
                .input
                .fill_buf()
                .map_err(|error| io_err(self.path, error))?;
            let count = buffer
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if count == 0 {
                return Ok(total);
            }
            self.input.consume(count);
            self.offset += count as u64;
            total += count;
        }
    }
    fn number(&mut self) -> Result<()> {
        if self.peek()? == Some(b'-') {
            self.byte()?;
        }
        match self.peek()? {
            Some(b'0') => {
                self.byte()?;
            }
            Some(b'1'..=b'9') => {
                self.digits()?;
            }
            _ => return Err(format_error(self.path, "数字无效")),
        }
        if self.peek()? == Some(b'.') {
            self.byte()?;
            if self.digits()? == 0 {
                return Err(format_error(self.path, "小数无效"));
            }
        }
        if matches!(self.peek()?, Some(b'e' | b'E')) {
            self.byte()?;
            if matches!(self.peek()?, Some(b'+' | b'-')) {
                self.byte()?;
            }
            if self.digits()? == 0 {
                return Err(format_error(self.path, "指数无效"));
            }
        }
        Ok(())
    }
    fn value(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_JSON_DEPTH {
            return Err(format_error(self.path, "JSON 嵌套过深"));
        }
        self.whitespace()?;
        match self.peek()? {
            Some(b'"') => {
                self.string(RawCapture::ignored())?;
            }
            Some(b'{') => {
                self.byte()?;
                self.whitespace()?;
                if self.peek()? == Some(b'}') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.whitespace()?;
                    self.string(RawCapture::ignored())?;
                    self.whitespace()?;
                    self.expect(b':')?;
                    self.value(depth + 1)?;
                    if self.object_end()? {
                        break;
                    }
                }
            }
            Some(b'[') => {
                self.byte()?;
                self.whitespace()?;
                if self.peek()? == Some(b']') {
                    self.byte()?;
                    return Ok(());
                }
                loop {
                    self.value(depth + 1)?;
                    self.whitespace()?;
                    match self.byte()? {
                        b']' => break,
                        b',' => {}
                        _ => return Err(format_error(self.path, "数组分隔符无效")),
                    }
                }
            }
            Some(b't') => self.literal(b"true")?,
            Some(b'f') => self.literal(b"false")?,
            Some(b'n') => self.literal(b"null")?,
            Some(b'-' | b'0'..=b'9') => self.number()?,
            _ => return Err(format_error(self.path, "JSON 值无效")),
        }
        Ok(())
    }
    fn object_end(&mut self) -> Result<bool> {
        self.whitespace()?;
        match self.byte()? {
            b'}' => Ok(true),
            b',' => Ok(false),
            _ => Err(format_error(self.path, "对象分隔符无效")),
        }
    }
    fn object_start(&mut self) -> Result<bool> {
        self.whitespace()?;
        self.expect(b'{')?;
        self.whitespace()?;
        if self.peek()? == Some(b'}') {
            self.byte()?;
            Ok(false)
        } else {
            Ok(true)
        }
    }
    fn record_kind(&mut self) -> Result<RecordKind> {
        self.whitespace()?;
        if self.peek()?.is_none() {
            return Ok(RecordKind::Blank);
        }
        if self.peek()? != Some(b'{') {
            self.value(0)?;
            self.end()?;
            return Ok(RecordKind::Other);
        }
        let mut kind = None;
        if self.object_start()? {
            loop {
                self.whitespace()?;
                let key = self.key()?;
                self.whitespace()?;
                self.expect(b':')?;
                if key.as_deref() == Some("type") {
                    // Only this exact short tag identifies metadata. Arbitrarily
                    // long ordinary event types are streamed past as well.
                    kind = self.short_string("session_meta".len(), false, 1)?.value;
                } else {
                    self.value(1)?;
                }
                if self.object_end()? {
                    break;
                }
            }
        }
        self.end()?;
        Ok(if kind.as_deref() == Some("session_meta") {
            RecordKind::Metadata
        } else {
            RecordKind::Other
        })
    }
    fn source(&mut self, depth: usize) -> Result<bool> {
        self.whitespace()?;
        if self.peek()? == Some(b'"') {
            return Ok(self
                .short_string(MAX_METADATA_STRING_BYTES, true, depth)?
                .value
                .as_deref()
                .is_some_and(source_is_internal));
        }
        if self.peek()? != Some(b'{') {
            self.value(depth)?;
            return Ok(false);
        }
        let mut internal = false;
        if self.object_start()? {
            loop {
                self.whitespace()?;
                let key = self.key()?;
                internal |= matches!(key.as_deref(), Some("subagent" | "internal"));
                self.whitespace()?;
                self.expect(b':')?;
                self.value(depth + 1)?;
                if self.object_end()? {
                    break;
                }
            }
        }
        Ok(internal)
    }
    fn payload(&mut self) -> Result<Option<Metadata>> {
        self.whitespace()?;
        if self.peek()? != Some(b'{') {
            self.value(1)?;
            return Ok(None);
        }
        self.expect(b'{')?;
        self.whitespace()?;
        let mut metadata = Metadata::default();
        if self.peek()? == Some(b'}') {
            metadata.close_offset = self.offset;
            self.byte()?;
            return Ok(Some(metadata));
        }
        loop {
            self.whitespace()?;
            let key = self.key()?;
            metadata.has_fields = true;
            self.whitespace()?;
            self.expect(b':')?;
            self.whitespace()?;
            let start = self.offset;
            match key.as_deref() {
                Some("id") => {
                    metadata.id = self.short_string(MAX_METADATA_STRING_BYTES, true, 2)?.value
                }
                Some("cwd") => metadata.cwd = self.short_string(MAX_CWD_BYTES, true, 2)?.value,
                Some("model_provider") => {
                    metadata.provider =
                        self.short_string(MAX_METADATA_STRING_BYTES, true, 2)?.value;
                    metadata.provider_span = Some(start..self.offset);
                }
                Some("source") => metadata.source_internal = self.source(2)?,
                Some("thread_source") => {
                    metadata.thread_source =
                        self.short_string(MAX_METADATA_STRING_BYTES, true, 2)?.value
                }
                _ => self.value(2)?,
            }
            self.whitespace()?;
            if self.peek()? == Some(b'}') {
                metadata.close_offset = self.offset;
                self.byte()?;
                break;
            }
            self.expect(b',')?;
        }
        Ok(Some(metadata))
    }
    fn metadata(&mut self) -> Result<Metadata> {
        let mut metadata = None;
        if self.object_start()? {
            loop {
                self.whitespace()?;
                let key = self.key()?;
                self.whitespace()?;
                self.expect(b':')?;
                if key.as_deref() == Some("payload") {
                    metadata = self.payload()?;
                } else {
                    self.value(1)?;
                }
                if self.object_end()? {
                    break;
                }
            }
        }
        self.end()?;
        metadata.ok_or_else(|| format_error(self.path, "session_meta 缺少对象 payload"))
    }
    fn matching_fields(
        &mut self,
        field_names: &[&str],
        expected: &HashSet<String>,
        limit: usize,
    ) -> Result<bool> {
        self.whitespace()?;
        if self.peek()?.is_none() {
            return Ok(false);
        }
        if self.peek()? != Some(b'{') {
            self.value(0)?;
            self.end()?;
            return Ok(false);
        }
        let mut values = HashMap::new();
        if self.object_start()? {
            loop {
                self.whitespace()?;
                let key = self.key()?;
                self.whitespace()?;
                self.expect(b':')?;
                if let Some(key) = key.filter(|key| field_names.contains(&key.as_str())) {
                    values.insert(key, self.short_string(limit, false, 1)?);
                } else {
                    self.value(1)?;
                }
                if self.object_end()? {
                    break;
                }
            }
        }
        self.end()?;
        for name in field_names {
            if let Some(value) = values.get(*name) {
                if value.is_string {
                    return Ok(value
                        .value
                        .as_ref()
                        .is_some_and(|value| expected.contains(value)));
                }
            }
        }
        Ok(false)
    }
}

struct ShortString {
    is_string: bool,
    value: Option<String>,
}
enum RecordKind {
    Blank,
    Metadata,
    Other,
}

#[derive(Default)]
struct Metadata {
    id: Option<String>,
    cwd: Option<String>,
    provider: Option<String>,
    source_internal: bool,
    thread_source: Option<String>,
    provider_span: Option<Range<u64>>,
    close_offset: u64,
    has_fields: bool,
}

fn source_is_internal(source: &str) -> bool {
    let source = source.trim().to_ascii_lowercase();
    source == "internal"
        || source == "subagent"
        || source.starts_with("internal_")
        || source.starts_with("subagent_")
}

fn thread_source_is_internal(source: &str) -> bool {
    matches!(
        source.trim().to_ascii_lowercase().as_str(),
        "subagent" | "guardian_review" | "memory_consolidation"
    ) || source_is_internal(source)
        || serde_json::from_str::<serde_json::Value>(source)
            .ok()
            .as_ref()
            .is_some_and(|value| match value {
                serde_json::Value::String(source) => source_is_internal(source),
                serde_json::Value::Object(source) => {
                    source.contains_key("internal") || source.contains_key("subagent")
                }
                _ => false,
            })
}

pub(super) fn record_matches_top_level_strings(
    record: &mut File,
    field_names: &[&str],
    expected: &HashSet<String>,
    max_decoded_bytes: usize,
) -> Result<bool> {
    let path = Path::new("JSONL record");
    record
        .seek(SeekFrom::Start(0))
        .map_err(|error| io_err(path, error))?;
    JsonReader::new(record, path).matching_fields(field_names, expected, max_decoded_bytes)
}

fn copy_hashed<R: Read>(
    mut reader: R,
    output: &mut File,
    hash: &mut Sha256,
    path: &Path,
) -> Result<u64> {
    let mut buffer = [0u8; IO_BUFFER_BYTES];
    let mut total = 0;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| io_err(path, error))?;
        if count == 0 {
            return Ok(total);
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| io_err(path, error))?;
        hash.update(&buffer[..count]);
        total += count as u64;
    }
}

fn patched_record(
    record: &mut File,
    metadata: &Metadata,
    provider: &[u8],
    next: &mut File,
    hash: &mut Sha256,
    path: &Path,
) -> Result<()> {
    let (start, end) = metadata
        .provider_span
        .as_ref()
        .map(|span| (span.start, span.end))
        .unwrap_or((metadata.close_offset, metadata.close_offset));
    record
        .seek(SeekFrom::Start(0))
        .map_err(|error| io_err(path, error))?;
    if copy_hashed((&mut *record).take(start), next, hash, path)? != start {
        return Err(format_error(path, "元数据改写范围无效"));
    }
    if metadata.provider_span.is_none() {
        let key = if metadata.has_fields {
            &b",\"model_provider\":"[..]
        } else {
            &b"\"model_provider\":"[..]
        };
        next.write_all(key).map_err(|error| io_err(path, error))?;
        hash.update(key);
    }
    next.write_all(provider)
        .map_err(|error| io_err(path, error))?;
    hash.update(provider);
    record
        .seek(SeekFrom::Start(end))
        .map_err(|error| io_err(path, error))?;
    copy_hashed(record, next, hash, path)?;
    Ok(())
}

fn first_meta_from_metadata(metadata: &Metadata, path: &Path) -> Result<FirstMeta> {
    let id = metadata
        .id
        .as_ref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| format_error(path, "起始 session_meta 缺少线程 ID"))?;
    Ok(FirstMeta {
        id: id.clone(),
        cwd: metadata.cwd.clone(),
        provider: metadata.provider.clone(),
        is_internal: metadata.source_internal
            || metadata
                .thread_source
                .as_deref()
                .is_some_and(thread_source_is_internal),
    })
}

/// Read just the first nonblank JSONL record. Fixed-buffer read-ahead may fetch
/// bytes after its LF, but no later record is spooled, parsed or validated.
pub(super) fn read_first_meta<R: Read>(reader: R, path: &Path) -> Result<FirstMeta> {
    let directory = private_directory("codex-x-first-meta-", path)?;
    let spool_path = directory.path().join("first.jsonl");
    let mut spool = private_file(&spool_path)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, reader);
    let mut record_bytes = 0u64;
    loop {
        let buffer = reader.fill_buf().map_err(|error| io_err(path, error))?;
        let eof = buffer.is_empty();
        if eof && record_bytes == 0 {
            return Err(format_error(path, "缺少起始 session_meta"));
        }
        let ending = buffer.iter().position(|byte| *byte == b'\n');
        let count = ending.map_or(buffer.len(), |index| index + 1);
        spool
            .write_all(&buffer[..count])
            .map_err(|error| io_err(&spool_path, error))?;
        record_bytes = record_bytes
            .checked_add(count as u64)
            .ok_or_else(|| format_error(path, "记录大小溢出"))?;
        reader.consume(count);
        if ending.is_some() || eof {
            spool
                .seek(SeekFrom::Start(0))
                .map_err(|error| io_err(&spool_path, error))?;
            match JsonReader::new(&mut spool, path).record_kind()? {
                RecordKind::Blank => {
                    spool
                        .set_len(0)
                        .map_err(|error| io_err(&spool_path, error))?;
                    spool
                        .seek(SeekFrom::Start(0))
                        .map_err(|error| io_err(&spool_path, error))?;
                    record_bytes = 0;
                }
                RecordKind::Other => return Err(format_error(path, "缺少起始 session_meta")),
                RecordKind::Metadata => {
                    spool
                        .seek(SeekFrom::Start(0))
                        .map_err(|error| io_err(&spool_path, error))?;
                    let metadata = JsonReader::new(&mut spool, path).metadata()?;
                    return first_meta_from_metadata(&metadata, path);
                }
            }
        }
    }
}

/// Validate the whole rollout and stage byte-preserving Provider patches. The
/// only retained buffers are fixed IO buffers and bounded identity fields.
pub(super) fn scan_rollout_stream(
    file: &mut File,
    path: &Path,
    target_provider: &str,
) -> Result<StreamedRollout> {
    if target_provider.len() > MAX_METADATA_STRING_BYTES {
        return Err(format_error(path, "目标 Provider 标识过长"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_err(path, error))?;
    let directory = private_directory("codex-x-rollout-snapshot-", path)?;
    let original_path = directory.path().join("original.jsonl");
    let next_path = directory.path().join("next.jsonl");
    let mut original = private_file(&original_path)?;
    let mut next = private_file(&next_path)?;
    let provider_json = serde_json::to_vec(target_provider)
        .map_err(|_| format_error(path, "目标 Provider 无法编码"))?;
    let mut original_hash = Sha256::new();
    let mut next_hash = Sha256::new();
    let mut first_meta = None;
    let mut session_meta_count = 0;
    let mut mismatch_count = 0;
    frame_records(&mut *file, directory.path(), path, |record, _length| {
        record
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_err(path, error))?;
        copy_hashed(&mut *record, &mut original, &mut original_hash, path)?;
        record
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_err(path, error))?;
        let kind = JsonReader::new(&mut *record, path).record_kind()?;
        if matches!(kind, RecordKind::Other) && first_meta.is_none() {
            return Err(format_error(path, "缺少起始 session_meta"));
        }
        if matches!(kind, RecordKind::Metadata) {
            record
                .seek(SeekFrom::Start(0))
                .map_err(|error| io_err(path, error))?;
            let metadata = JsonReader::new(&mut *record, path).metadata()?;
            session_meta_count += 1;
            if first_meta.is_none() {
                first_meta = Some(first_meta_from_metadata(&metadata, path)?);
            }
            if metadata.provider.as_deref() != Some(target_provider) {
                mismatch_count += 1;
                patched_record(
                    record,
                    &metadata,
                    &provider_json,
                    &mut next,
                    &mut next_hash,
                    path,
                )?;
                return Ok(());
            }
        }
        record
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_err(path, error))?;
        copy_hashed(record, &mut next, &mut next_hash, path)?;
        Ok(())
    })?;
    let first_meta = first_meta.ok_or_else(|| format_error(path, "缺少起始 session_meta"))?;
    let original_mtime = file
        .metadata()
        .map_err(|error| io_err(path, error))?
        .modified()
        .ok();
    let original_hash: [u8; 32] = original_hash.finalize().into();
    let next_hash: [u8; 32] = next_hash.finalize().into();
    let staged = if mismatch_count != 0 {
        original
            .sync_all()
            .map_err(|error| io_err(&original_path, error))?;
        next.sync_all().map_err(|error| io_err(&next_path, error))?;
        drop(original);
        drop(next);
        Some(Arc::new(StreamedSnapshot {
            _directory: directory,
            original_path,
            next_path,
            original_hash,
            next_hash,
        }))
    } else {
        None
    };
    Ok(StreamedRollout {
        first_meta,
        session_meta_count,
        mismatch_count,
        original_hash,
        original_mtime,
        staged,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: TempDir,
        path: PathBuf,
    }

    impl Fixture {
        fn bytes(bytes: &[u8]) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("rollout-synthetic.jsonl");
            fs::write(&path, bytes).unwrap();
            Self {
                _directory: directory,
                path,
            }
        }
        fn scan(&self, target: &str) -> Result<StreamedRollout> {
            scan_rollout_stream(&mut File::open(&self.path).unwrap(), &self.path, target)
        }
    }

    fn file_hash(path: &Path) -> [u8; 32] {
        let mut file = File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut buffer = [0; IO_BUFFER_BYTES];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        hash.finalize().into()
    }

    fn repeat_bytes(file: &mut File, byte: u8, count: usize) {
        let buffer = [byte; IO_BUFFER_BYTES];
        let mut remaining = count;
        while remaining != 0 {
            let length = remaining.min(buffer.len());
            file.write_all(&buffer[..length]).unwrap();
            remaining -= length;
        }
    }

    #[test]
    fn provider_patch_preserves_other_bytes_crlf_and_missing_final_lf() {
        let original = b"\r\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"cwd\":\"/tmp/work\",\"model_provider\" : \"openai\",\"source\":\"vscode\"}}\r\n{\"type\":\"event_msg\",\"payload\":{\"text\":\"openai chat content \\\\n\"}}";
        let fixture = Fixture::bytes(original);
        let result = fixture.scan("custom").unwrap();
        assert_eq!(result.first_meta.id, "thread");
        assert_eq!(result.first_meta.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(result.first_meta.provider.as_deref(), Some("openai"));
        assert!(!result.first_meta.is_internal);
        assert_eq!(result.session_meta_count, 1);
        assert_eq!(result.mismatch_count, 1);
        let snapshot = result.staged.as_ref().unwrap();
        let expected = String::from_utf8(original.to_vec()).unwrap().replacen(
            "\"model_provider\" : \"openai\"",
            "\"model_provider\" : \"custom\"",
            1,
        );
        assert_eq!(fs::read(&snapshot.original_path).unwrap(), original);
        assert_eq!(fs::read(&snapshot.next_path).unwrap(), expected.as_bytes());
        assert_eq!(snapshot.original_hash, file_hash(&fixture.path));
        assert_eq!(snapshot.next_hash, file_hash(&snapshot.next_path));
        assert_eq!(result.original_hash, snapshot.original_hash);
        assert!(result.original_mtime.is_some());
        assert_eq!(fs::read(&fixture.path).unwrap(), original);
        assert!(!snapshot
            .original_path
            .parent()
            .unwrap()
            .join("record.jsonl")
            .exists());
    }

    #[test]
    fn matching_rollout_retains_hash_without_snapshots() {
        let bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"model_provider\":\"custom\"}}\n{\"type\":\"event_msg\",\"payload\":true}\n";
        let fixture = Fixture::bytes(bytes);
        let result = fixture.scan("custom").unwrap();
        assert_eq!(result.mismatch_count, 0);
        assert!(result.staged.is_none());
        assert_eq!(result.original_hash, file_hash(&fixture.path));
    }

    #[test]
    fn ordinary_arrays_null_blank_records_and_long_event_types_have_no_size_rejection() {
        let fixture = Fixture::bytes(b"");
        let mut source = File::create(&fixture.path).unwrap();
        source.write_all(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"model_provider\":\"custom\"}}\n[null,true,{\"ignored\":false}]\r\nnull\n \t\r\n{\"type\":\"").unwrap();
        repeat_bytes(&mut source, b'x', 1024 * 1024);
        source
            .write_all(b"\",\"unknown\":{\"huge\":null}}")
            .unwrap();
        drop(source);
        let result = fixture.scan("custom").unwrap();
        assert_eq!(result.session_meta_count, 1);
        assert_eq!(result.mismatch_count, 0);
        assert!(result.staged.is_none());
        assert_eq!(result.original_hash, file_hash(&fixture.path));
    }

    #[test]
    fn first_meta_stops_before_invalid_body_and_after_one_reader_record() {
        let first = b"\r\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"internal\",\"source\":{\"internal\":true}}}\r\n";
        let mut bytes = first.to_vec();
        bytes.extend_from_slice(b"\xffnot valid JSON body");
        let metadata = read_first_meta(&bytes[..], Path::new("synthetic-first-only")).unwrap();
        assert_eq!(metadata.id, "internal");
        assert!(metadata.is_internal);

        struct OneRecordReader {
            bytes: Option<&'static [u8]>,
        }
        impl Read for OneRecordReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let bytes = self
                    .bytes
                    .take()
                    .ok_or_else(|| std::io::Error::other("body must not be read"))?;
                assert!(buffer.len() >= bytes.len());
                buffer[..bytes.len()].copy_from_slice(bytes);
                Ok(bytes.len())
            }
        }
        let metadata = read_first_meta(
            OneRecordReader { bytes: Some(first) },
            Path::new("synthetic-read-stop"),
        )
        .unwrap();
        assert_eq!(metadata.id, "internal");
        assert!(metadata.is_internal);
    }

    #[test]
    fn lexical_validation_agrees_with_json_for_small_syntax_and_unicode_cases() {
        for bytes in [
            &br#"{"x":[null,true,false,-1.25e+2,{"y":"\uD83D\uDE00"}]}"#[..],
            &br#"{"x":"escaped \\ \" \/ \b \f \n \r \t"}"#[..],
            &br#"[1,2,3]"#[..],
            &br#"{}"#[..],
            &br#"null"#[..],
            &br#"{"x":01}"#[..],
            &br#"{"x":1.}"#[..],
            &br#"{"x":1e}"#[..],
            &br#"{"x":truex}"#[..],
            &br#"{"x":[1,]}"#[..],
            &br#"{"x":1,}"#[..],
            &br#"{"x":"\uDE00"}"#[..],
            &br#"{"x":"\uD83D\u0041"}"#[..],
            &b"{\"x\":\"\xf0\x80\x80\x80\"}"[..],
        ] {
            let expected = serde_json::from_slice::<serde_json::Value>(bytes).is_ok();
            let actual = JsonReader::new(bytes, Path::new("synthetic-syntax"))
                .record_kind()
                .is_ok();
            assert_eq!(actual, expected, "synthetic JSON syntax case");
        }
    }

    #[test]
    fn patches_missing_nonstring_escaped_and_duplicate_provider_fields() {
        for (input, expected) in [
            (
                r#"{"type":"session_meta","payload":{"id":"t","other":7}}"#,
                r#"{"type":"session_meta","payload":{"id":"t","other":7,"model_provider":"custom"}}"#,
            ),
            (
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":[1,{"x":true}],"other":7}}"#,
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":"custom","other":7}}"#,
            ),
            (
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":null}}"#,
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":"custom"}}"#,
            ),
            (
                r#"{"ty\u0070e":"session_meta","pay\u006coad":{"id":"t","model_\u0070rovider":"old"}}"#,
                r#"{"ty\u0070e":"session_meta","pay\u006coad":{"id":"t","model_\u0070rovider":"custom"}}"#,
            ),
            (
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":"custom","model_provider":"old"}}"#,
                r#"{"type":"session_meta","payload":{"id":"t","model_provider":"custom","model_provider":"custom"}}"#,
            ),
        ] {
            let fixture = Fixture::bytes(input.as_bytes());
            let result = fixture.scan("custom").unwrap();
            assert_eq!(result.mismatch_count, 1);
            assert_eq!(
                fs::read(result.staged.unwrap().next_path.clone()).unwrap(),
                expected.as_bytes()
            );
        }
    }

    #[test]
    fn replayed_metadata_patches_each_provider_and_only_first_record_classifies_source() {
        let bytes = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"user\",\"source\":\"vscode\",\"model_provider\":\"old\"}}\n{\"type\":\"session_meta\",\"payload\":{\"id\":\"parent\",\"source\":{\"internal\":\"guardian\"}}}\n{\"type\":\"session_meta\",\"payload\":{}}";
        let fixture = Fixture::bytes(bytes);
        let result = fixture.scan("custom").unwrap();
        assert_eq!(result.session_meta_count, 3);
        assert_eq!(result.mismatch_count, 3);
        assert_eq!(result.first_meta.id, "user");
        assert!(!result.first_meta.is_internal);
        let next = fs::read(result.staged.unwrap().next_path.clone()).unwrap();
        assert!(std::str::from_utf8(&next)
            .unwrap()
            .ends_with(r#"{"type":"session_meta","payload":{"model_provider":"custom"}}"#));
    }

    #[test]
    fn internal_sources_and_unicode_metadata_follow_existing_classification() {
        for source in [r#""internal_guardian""#, r#"{"subagent":{"unknown":true}}"#] {
            let bytes = format!(
                r#"{{"type":"session_meta","payload":{{"id":"线程","cwd":"/tmp/项目","model_provider":"custom","source":{source}}}}}"#
            );
            let result = Fixture::bytes(bytes.as_bytes()).scan("custom").unwrap();
            assert!(result.first_meta.is_internal);
            assert_eq!(result.first_meta.id, "线程");
            assert_eq!(result.first_meta.cwd.as_deref(), Some("/tmp/项目"));
        }
    }

    #[test]
    fn invalid_json_utf8_and_missing_identity_do_not_produce_stages() {
        for bytes in [
            &b"{\"type\":\"event_msg\",\"payload\":true}"[..],
            &b"{\"type\":\"session_meta\",\"payload\":{}}"[..],
            &b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"t\"}}\n{\"x\":1,}"[..],
            &b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"t\"}}\n{\"x\":\"\xff\"}"[..],
            &b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"t\"}}\n{\"x\":\"\\uD800\"}"[..],
        ] {
            let fixture = Fixture::bytes(bytes);
            assert!(matches!(
                fixture.scan("custom"),
                Err(CodexxError::Config(_))
            ));
            assert_eq!(fs::read(&fixture.path).unwrap(), bytes);
        }
    }

    #[test]
    fn record_matcher_uses_last_duplicate_value_and_bounds_selected_strings() {
        let expected = HashSet::from(["selected".to_string()]);
        for (bytes, matched) in [
            (r#"{"id":"selected","id":"keep"}"#, false),
            (r#"{"id":"keep","id":"selected"}"#, true),
            (r#"{"id":"selected","id":false}"#, false),
            (r#"{"i\u0064":"selected","other":[true,null,2.1e-3]}"#, true),
            (r#"{"id":"selected-but-too-long"}"#, false),
        ] {
            let fixture = Fixture::bytes(bytes.as_bytes());
            assert_eq!(
                record_matches_top_level_strings(
                    &mut File::open(&fixture.path).unwrap(),
                    &["id"],
                    &expected,
                    8
                )
                .unwrap(),
                matched
            );
        }
        let fixture = Fixture::bytes(br#"{"id":"selected",}"#);
        assert!(matches!(
            record_matches_top_level_strings(
                &mut File::open(&fixture.path).unwrap(),
                &["id"],
                &expected,
                128
            ),
            Err(CodexxError::Config(_))
        ));
    }

    #[test]
    fn record_framing_preserves_blank_crlf_and_last_record_and_propagates_failures() {
        let bytes = b"\r\n{\"x\":1}\r\n{\"x\":2}";
        let mut records = Vec::new();
        for_each_jsonl_record(&bytes[..], |record, length| {
            let mut bytes = Vec::new();
            record.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len() as u64, length);
            records.push(bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            records,
            vec![
                b"\r\n".to_vec(),
                b"{\"x\":1}\r\n".to_vec(),
                b"{\"x\":2}".to_vec()
            ]
        );
        let result = for_each_jsonl_record(&bytes[..], |_record, _length| {
            Err(CodexxError::Config("synthetic callback failure".into()))
        });
        assert!(matches!(result, Err(CodexxError::Config(_))));
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic IO failure"))
            }
        }
        assert!(matches!(
            for_each_jsonl_record(FailedReader, |_, _| Ok(())),
            Err(CodexxError::Io { .. })
        ));
    }

    #[test]
    fn reused_record_spool_preserves_equal_shorter_and_longer_records_exactly() {
        // Cover equal lengths, shrinking (including final unterminated EOF),
        // growth across IO-buffer boundaries, and an empty CRLF record.
        let mut large = vec![b'x'; IO_BUFFER_BYTES * 2 + 17];
        large.push(b'\n');
        let records = vec![
            b"first same\n".to_vec(),
            b"other same\n".to_vec(),
            b"x\n".to_vec(),
            large,
            b"\r\n".to_vec(),
            b"grows again\n".to_vec(),
            b"end".to_vec(),
        ];
        let input = records.concat();
        let mut index = 0;
        for_each_jsonl_record(&input[..], |record, length| {
            assert_eq!(length as usize, records[index].len());
            assert_eq!(record.metadata().unwrap().len(), length);
            let mut actual = Vec::new();
            record.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, records[index]);
            // Leave the cursor somewhere other than EOF to exercise reset.
            record.seek(SeekFrom::Start(1)).unwrap();
            index += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(index, records.len());
    }

    #[test]
    fn huge_nonmetadata_record_is_validated_and_preserved_without_whole_line_buffers() {
        let fixture = Fixture::bytes(b"");
        let mut source = File::create(&fixture.path).unwrap();
        source.write_all(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"large\",\"model_provider\":\"old\"}}\r\n{\"type\":\"response_item\",\"payload\":{\"image\":\"").unwrap();
        repeat_bytes(&mut source, b'x', 80 * 1024 * 1024);
        source.write_all(b"\"}}\r\n").unwrap();
        drop(source);
        let result = fixture.scan("new").unwrap();
        let snapshot = result.staged.unwrap();
        assert_eq!(result.session_meta_count, 1);
        assert_eq!(result.mismatch_count, 1);
        assert_eq!(snapshot.original_hash, file_hash(&fixture.path));
        assert_eq!(snapshot.original_hash, file_hash(&snapshot.original_path));
        // Equal-width Provider replacement leaves every other byte and size.
        assert_eq!(
            fs::metadata(&fixture.path).unwrap().len(),
            fs::metadata(&snapshot.next_path).unwrap().len()
        );
        let rescanned = scan_rollout_stream(
            &mut File::open(&snapshot.next_path).unwrap(),
            &snapshot.next_path,
            "new",
        )
        .unwrap();
        assert_eq!(rescanned.mismatch_count, 0);
        assert!(rescanned.staged.is_none());
    }

    #[test]
    fn huge_metadata_instructions_and_unknown_keys_are_streamed_past() {
        let fixture = Fixture::bytes(b"");
        let mut source = File::create(&fixture.path).unwrap();
        source
            .write_all(b"{\"payload\":{\"id\":\"large-meta\",\"instructions\":\"")
            .unwrap();
        repeat_bytes(&mut source, b'x', 80 * 1024 * 1024);
        source
            .write_all(b"\",\"model_provider\":\"old\"},\"type\":\"session_meta\",\"")
            .unwrap();
        repeat_bytes(&mut source, b'k', 1024 * 1024);
        source.write_all(b"\":{\"ignored\":true}}").unwrap();
        drop(source);
        let result = fixture.scan("new").unwrap();
        assert_eq!(result.first_meta.id, "large-meta");
        assert_eq!(result.mismatch_count, 1);
        let snapshot = result.staged.unwrap();
        assert_eq!(snapshot.original_hash, file_hash(&fixture.path));
        assert_eq!(
            fs::metadata(&snapshot.original_path).unwrap().len(),
            fs::metadata(&snapshot.next_path).unwrap().len()
        );
        assert_eq!(snapshot.next_hash, file_hash(&snapshot.next_path));
    }

    #[cfg(unix)]
    #[test]
    fn staged_snapshots_are_private_and_owned_until_last_arc_is_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::bytes(
            br#"{"type":"session_meta","payload":{"id":"t","model_provider":"old"}}"#,
        );
        let snapshot = fixture.scan("new").unwrap().staged.unwrap();
        let path = snapshot.original_path.clone();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let second = snapshot.clone();
        drop(snapshot);
        assert!(path.exists());
        drop(second);
        assert!(!path.exists());
    }
}
