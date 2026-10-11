//! Where a result goes: inline rows for a small result, a file for a big one.
//!
//! lmsql is stateless: one process, one connection, one statement batch, nothing remembered
//! between calls. A big result is therefore never truncated and never paged; it is written in
//! full to a file and the caller is told where it is.
//!
//! - `out` given: every row is streamed to that file (`ndjson`, `csv` or `json`), and the
//!   response is a summary with a preview.
//! - `out` not given: rows are kept inline up to `maxRows` / `maxBytes`. The first row that would
//!   pass either threshold switches the call to a *spill*: the rows so far and every following row
//!   go to a new ndjson file under `$LMSQL_OUT_DIR` (default `<temp>/lmsql/`), and the response is
//!   the same summary with `spilled: true`.
//!
//! Rows are written as they are fetched (bounded memory). A file is written to a temporary name in
//! the target directory (`create_new`, mode 0600) and renamed into place only when the call
//! succeeds; on any error the temporary file is removed and the target is left as it was. lmsql
//! never deletes old spill files: cleaning them up is the caller's job.

use crate::backend::{Column, RunStats};
use crate::error::DbError;
use lmrust::server::ErrorCode;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Preview rows in a file response when `maxRows` is not given.
pub const DEFAULT_PREVIEW_ROWS: usize = 20;
/// Spill directory override.
pub const OUT_DIR_ENV: &str = "LMSQL_OUT_DIR";

/// A result file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
  Ndjson,
  Csv,
  Json,
}

impl Format {
  pub const NAMES: [&'static str; 3] = ["ndjson", "csv", "json"];

  pub fn as_str(self) -> &'static str {
    match self {
      Format::Ndjson => "ndjson",
      Format::Csv => "csv",
      Format::Json => "json",
    }
  }

  pub fn parse(name: &str) -> Option<Format> {
    match name.to_ascii_lowercase().as_str() {
      "ndjson" | "jsonl" => Some(Format::Ndjson),
      "csv" => Some(Format::Csv),
      "json" => Some(Format::Json),
      _ => None,
    }
  }

  /// From the file extension: `.csv`, `.json`, `.ndjson`/`.jsonl`; anything else is ndjson.
  pub fn infer(path: &Path) -> Format {
    path
      .extension()
      .and_then(|e| e.to_str())
      .and_then(Format::parse)
      .unwrap_or(Format::Ndjson)
  }
}

fn out_error(message: String) -> DbError {
  DbError::new(ErrorCode::Forbidden, "OUT_NOT_WRITABLE", message)
}

fn io_error(what: &str, path: &Path, e: &io::Error) -> DbError {
  out_error(format!("cannot {what} {}: {e}", path.display()))
}

/// How one call delivers its result.
#[derive(Debug, Clone)]
pub struct OutputConfig {
  /// Explicit `out`: absolute path and format.
  pub out: Option<(PathBuf, Format)>,
  /// Inline thresholds (`maxRows` / `maxBytes`): above either, the result spills to a file.
  pub inline_rows: usize,
  pub inline_bytes: usize,
  /// Rows of the preview in a file response.
  pub preview_rows: usize,
  /// Dry run: no file is ever written (out is reported as `would_write`).
  pub dry_run: bool,
  /// The settings file's `output.dir`: `out` must resolve inside it, and spills go there.
  pub root: Option<PathBuf>,
}

impl OutputConfig {
  /// Validate `out` / `outFormat` and check, before any SQL runs, that the file can be created.
  pub fn new(
    out: Option<&str>,
    out_format: Option<&str>,
    inline_rows: usize,
    inline_bytes: usize,
    max_rows_given: bool,
    dry_run: bool,
    root: Option<&Path>,
  ) -> Result<OutputConfig, DbError> {
    let format = match out_format {
      None => None,
      Some(name) => Some(Format::parse(name).ok_or_else(|| {
        DbError::new(
          ErrorCode::ValidationError,
          "INVALID_OUT_FORMAT",
          format!("outFormat must be one of {}", Format::NAMES.join(", ")),
        )
      })?),
    };
    let out = match out {
      None => {
        if format.is_some() {
          return Err(DbError::new(
            ErrorCode::ValidationError,
            "OUT_FORMAT_WITHOUT_OUT",
            "outFormat applies to out; spill files are always ndjson",
          ));
        }
        None
      }
      Some(raw) => {
        let path = absolute(raw)?;
        if let Some(root) = root {
          contain(root, &path)?;
        }
        if !dry_run {
          preflight(&path)?;
        }
        let format = format.unwrap_or_else(|| Format::infer(&path));
        Some((path, format))
      }
    };
    Ok(OutputConfig {
      out,
      inline_rows,
      inline_bytes,
      preview_rows: if max_rows_given {
        inline_rows
      } else {
        DEFAULT_PREVIEW_ROWS.min(inline_rows)
      },
      dry_run,
      root: root.map(Path::to_path_buf),
    })
  }
}

/// Where `path` really is: canonical (symlinks and `..` followed). For a path that does not exist
/// yet, the canonical nearest existing ancestor plus the remaining names (`None` if a remaining
/// component is `..`, which cannot be resolved).
fn resolve_real(path: &Path) -> Option<PathBuf> {
  let mut rest = Vec::new();
  let mut current = path.to_path_buf();
  for _ in 0..64 {
    if let Ok(mut real) = current.canonicalize() {
      for name in rest.iter().rev() {
        real.push(name);
      }
      return Some(real);
    }
    // A dangling symlink: follow it to where it points.
    if let Ok(target) = std::fs::read_link(&current) {
      current = match target.is_absolute() {
        true => target,
        false => current.parent()?.join(target),
      };
      continue;
    }
    rest.push(current.file_name()?.to_os_string());
    current = current.parent()?.to_path_buf();
  }
  None
}

/// `output.dir` containment: `path` must resolve inside `root`.
fn contain(root: &Path, path: &Path) -> Result<(), DbError> {
  let inside = match (resolve_real(root), resolve_real(path)) {
    (Some(root), Some(real)) => real.starts_with(&root) && real != root,
    _ => false,
  };
  if inside {
    Ok(())
  } else {
    Err(DbError::new(
      ErrorCode::Forbidden,
      "PERMISSION_DENIED",
      format!(
        "permission denied: out {} is outside output.dir {} (settings file); write under that directory",
        path.display(),
        root.display()
      ),
    ))
  }
}

fn absolute(raw: &str) -> Result<PathBuf, DbError> {
  if raw.trim().is_empty() || raw.ends_with('/') {
    return Err(out_error("out must name a file".to_string()));
  }
  std::path::absolute(raw).map_err(|e| io_error("resolve", Path::new(raw), &e))
}

/// Create the parent directory if missing and prove a file can be created there (the probe is
/// removed). An existing directory at the path itself is refused.
fn preflight(path: &Path) -> Result<(), DbError> {
  if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
    return Err(out_error(format!(
      "out names a directory: {}",
      path.display()
    )));
  }
  let dir = parent(path);
  std::fs::create_dir_all(dir).map_err(|e| io_error("create directory", dir, &e))?;
  let (probe, file) = create_temp(dir, file_name(path))?;
  drop(file);
  let _ = std::fs::remove_file(probe);
  Ok(())
}

fn parent(path: &Path) -> &Path {
  path.parent().unwrap_or(Path::new("/"))
}

fn file_name(path: &Path) -> &str {
  path.file_name().and_then(|n| n.to_str()).unwrap_or("out")
}

/// 16 random hex digits (std's per-process random hash keys; no extra dependency).
fn random_hex() -> String {
  use std::hash::{BuildHasher, Hasher};
  let mut h = std::collections::hash_map::RandomState::new().build_hasher();
  h.write_u128(
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.as_nanos())
      .unwrap_or(0),
  );
  h.write_u32(std::process::id());
  format!("{:016x}", h.finish())
}

/// A new file next to the target, never an existing one, readable by the owner only.
fn create_temp(dir: &Path, name: &str) -> Result<(PathBuf, File), DbError> {
  let temp = dir.join(format!(".{name}.lmsql-{}.tmp", random_hex()));
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  let file = options
    .open(&temp)
    .map_err(|e| io_error("create", &dir.join(name), &e))?;
  Ok((temp, file))
}

/// The settings file's `output.dir` if set, else `$LMSQL_OUT_DIR`, else `<temp>/lmsql` (created
/// 0700; the default must be a real directory owned by this user and closed to others, since the
/// system temp directory is shared).
pub fn spill_dir(root: Option<&Path>) -> Result<PathBuf, DbError> {
  let configured = match root {
    Some(root) => Some(root.display().to_string()),
    None => std::env::var(OUT_DIR_ENV).ok().filter(|d| !d.is_empty()),
  };
  let default = configured.is_none();
  let dir = match configured {
    Some(d) => std::path::absolute(&d).map_err(|e| io_error("resolve", Path::new(&d), &e))?,
    None => std::env::temp_dir().join("lmsql"),
  };
  let mut builder = std::fs::DirBuilder::new();
  builder.recursive(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    builder.mode(0o700);
  }
  builder
    .create(&dir)
    .map_err(|e| io_error("create directory", &dir, &e))?;
  let meta =
    std::fs::symlink_metadata(&dir).map_err(|e| io_error("inspect directory", &dir, &e))?;
  if !meta.is_dir() {
    return Err(out_error(format!(
      "spill directory is not a directory (a symlink or file): {}",
      dir.display()
    )));
  }
  #[cfg(unix)]
  if default {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me || meta.mode() & 0o077 != 0 {
      return Err(out_error(format!(
        "spill directory {} is not private to this user; set {OUT_DIR_ENV}",
        dir.display()
      )));
    }
  }
  let _ = default;
  Ok(dir)
}

/// `lmsql-<UTC yyyymmddThhmmssZ>-<random>.ndjson` in the spill directory.
fn spill_path(root: Option<&Path>) -> Result<PathBuf, DbError> {
  let secs = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  let (days, rem) = (secs / 86_400, secs % 86_400);
  let (y, m, d) = civil_from_days(days as i64);
  let stamp = format!(
    "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
    rem / 3600,
    rem % 3600 / 60,
    rem % 60
  );
  Ok(spill_dir(root)?.join(format!("lmsql-{stamp}-{}.ndjson", &random_hex()[..8])))
}

/// Days since 1970-01-01 -> (year, month, day) (H. Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
  let z = z + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
  let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
  (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Counts the bytes written through it.
struct Counting {
  file: File,
  bytes: u64,
}

impl Write for Counting {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let n = self.file.write(buf)?;
    self.bytes += n as u64;
    Ok(n)
  }
  fn flush(&mut self) -> io::Result<()> {
    self.file.flush()
  }
}

/// Object keys for the columns: the names, with a duplicate getting `_2`, `_3`, ...
pub fn column_keys(columns: &[Column]) -> Vec<String> {
  let mut seen = std::collections::HashSet::new();
  columns
    .iter()
    .map(|c| {
      let mut key = c.name.clone();
      let mut n = 2;
      while !seen.insert(key.clone()) {
        key = format!("{}_{n}", c.name);
        n += 1;
      }
      key
    })
    .collect()
}

/// One CSV field (RFC 4180): NULL is an empty field; an empty string is `""` so the two differ;
/// numbers and booleans as JSON text; objects (`$blob_base64`, json) as compact JSON text.
pub fn csv_field(value: &Value) -> String {
  let mut out = Vec::new();
  push_csv_field(&mut out, value);
  String::from_utf8(out).expect("utf-8")
}

fn push_csv_text(out: &mut Vec<u8>, text: &str) {
  if text.is_empty() || text.contains([',', '"', '\r', '\n']) {
    out.push(b'"');
    for part in text.split('"').enumerate() {
      if part.0 > 0 {
        out.extend_from_slice(b"\"\"");
      }
      out.extend_from_slice(part.1.as_bytes());
    }
    out.push(b'"');
  } else {
    out.extend_from_slice(text.as_bytes());
  }
}

fn push_csv_field(out: &mut Vec<u8>, value: &Value) {
  match value {
    Value::Null => {}
    Value::String(s) => push_csv_text(out, s),
    Value::Bool(_) | Value::Number(_) => {
      let _ = serde_json::to_writer(&mut *out, value);
    }
    Value::Array(_) | Value::Object(_) => push_csv_text(out, &value.to_string()),
  }
}

/// A result file being written: a temporary file, renamed onto `target` by [`FileWriter::commit`];
/// dropped uncommitted, the temporary file is removed.
struct FileWriter {
  target: PathBuf,
  temp: Option<PathBuf>,
  out: BufWriter<Counting>,
  format: Format,
  /// JSON-encoded keys (ndjson/json) or CSV-quoted names (csv).
  keys: Vec<String>,
  rows: u64,
  /// One encoded row, reused.
  line: Vec<u8>,
}

impl Drop for FileWriter {
  fn drop(&mut self) {
    if let Some(temp) = self.temp.take() {
      let _ = std::fs::remove_file(temp);
    }
  }
}

impl FileWriter {
  fn create(target: &Path, format: Format, columns: &[Column]) -> Result<FileWriter, DbError> {
    let dir = parent(target);
    let (temp, file) = create_temp(dir, file_name(target))?;
    let names = column_keys(columns);
    let keys = match format {
      Format::Csv => names.iter().map(|n| csv_field(&json!(n))).collect(),
      _ => names
        .iter()
        .map(|n| Value::from(n.as_str()).to_string())
        .collect(),
    };
    let mut writer = FileWriter {
      target: target.to_path_buf(),
      temp: Some(temp),
      out: BufWriter::with_capacity(1 << 16, Counting { file, bytes: 0 }),
      format,
      keys,
      rows: 0,
      line: Vec::with_capacity(256),
    };
    match format {
      Format::Csv if !columns.is_empty() => {
        let header = writer.keys.join(",");
        writer.put(header.as_bytes())?;
        writer.put(b"\r\n")?;
      }
      Format::Json => writer.put(b"[")?,
      _ => {}
    }
    Ok(writer)
  }

  fn put(&mut self, bytes: &[u8]) -> Result<(), DbError> {
    let target = &self.target;
    self
      .out
      .write_all(bytes)
      .map_err(|e| io_error("write", target, &e))
  }

  /// Encode `values` as one JSON object into `line`.
  fn object(line: &mut Vec<u8>, keys: &[String], values: &[Value]) {
    line.push(b'{');
    for (i, (key, value)) in keys.iter().zip(values).enumerate() {
      if i > 0 {
        line.push(b',');
      }
      line.extend_from_slice(key.as_bytes());
      line.push(b':');
      let _ = serde_json::to_writer(&mut *line, value);
    }
    line.push(b'}');
  }

  fn write_row(&mut self, values: &[Value]) -> Result<(), DbError> {
    let mut line = std::mem::take(&mut self.line);
    line.clear();
    match self.format {
      Format::Ndjson => {
        Self::object(&mut line, &self.keys, values);
        line.push(b'\n');
      }
      Format::Json => {
        line.extend_from_slice(if self.rows == 0 { b"\n" } else { b",\n" });
        Self::object(&mut line, &self.keys, values);
      }
      Format::Csv => {
        for (i, value) in values.iter().enumerate() {
          if i > 0 {
            line.push(b',');
          }
          push_csv_field(&mut line, value);
        }
        line.extend_from_slice(b"\r\n");
      }
    }
    let written = self.put(&line);
    self.line = line;
    written?;
    self.rows += 1;
    Ok(())
  }

  /// Finish the format, flush and sync, and rename onto the target.
  fn commit(mut self) -> Result<FileInfo, DbError> {
    if self.format == Format::Json {
      self.put(b"\n]\n")?;
    }
    let target = self.target.clone();
    self
      .out
      .flush()
      .map_err(|e| io_error("write", &target, &e))?;
    let counting = self.out.get_ref();
    counting
      .file
      .sync_all()
      .map_err(|e| io_error("sync", &target, &e))?;
    let bytes = counting.bytes;
    let overwrote = std::fs::symlink_metadata(&target).is_ok();
    let temp = self.temp.clone().expect("uncommitted");
    std::fs::rename(&temp, &target).map_err(|e| io_error("rename onto", &target, &e))?;
    self.temp = None;
    Ok(FileInfo {
      path: target,
      format: self.format,
      bytes,
      rows: self.rows,
      overwrote,
    })
  }
}

/// A committed result file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
  pub path: PathBuf,
  pub format: Format,
  pub bytes: u64,
  pub rows: u64,
  pub overwrote: bool,
}

/// Receives the rows of each result set as the backend fetches them. A new result set
/// ([`Sink::begin`]) replaces the previous one: the call returns its last statement's rows.
pub struct Sink {
  config: OutputConfig,
  columns: Vec<Column>,
  inline: Vec<Vec<Value>>,
  inline_bytes: usize,
  preview: Vec<Vec<Value>>,
  preview_bytes: usize,
  file: Option<FileWriter>,
  spilled: bool,
  /// Dry run: the result passed an inline threshold and would have spilled.
  would_spill: bool,
  row_count: u64,
}

/// What a call produced: inline rows, or a file with a preview.
#[derive(Debug, Clone, PartialEq)]
pub enum Collected {
  Inline {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
  },
  File {
    columns: Vec<Column>,
    row_count: u64,
    preview: Vec<Vec<Value>>,
    file: FileInfo,
    spilled: bool,
  },
  /// A dry run whose result would have gone to a file: nothing written, a preview only.
  DryRunFile {
    columns: Vec<Column>,
    row_count: u64,
    preview: Vec<Vec<Value>>,
    would_write: Option<(PathBuf, Format)>,
    would_spill: bool,
  },
}

/// Serialized size of a row, plus the `,` before it unless it is the first: with the 2 bytes of
/// `[]`, the running total is exactly the length of the serialized `rows` / `preview` array.
fn row_size(values: &[Value], first: bool) -> usize {
  serde_json::to_string(values)
    .map(|s| s.len())
    .unwrap_or(usize::MAX / 2)
    + usize::from(!first)
}

impl Sink {
  pub fn new(config: OutputConfig) -> Sink {
    Sink {
      config,
      columns: Vec::new(),
      inline: Vec::new(),
      inline_bytes: 2,
      preview: Vec::new(),
      preview_bytes: 2,
      file: None,
      spilled: false,
      would_spill: false,
      row_count: 0,
    }
  }

  /// A new result set (an empty `columns` for a statement without rows).
  pub fn begin(&mut self, columns: Vec<Column>) -> Result<(), DbError> {
    self.file = None; // an earlier result's temporary file is removed
    self.inline.clear();
    self.inline_bytes = 2;
    self.preview.clear();
    self.preview_bytes = 2;
    self.spilled = false;
    self.would_spill = false;
    self.row_count = 0;
    if let Some((path, format)) = &self.config.out {
      if !self.config.dry_run {
        self.file = Some(FileWriter::create(path, *format, &columns)?);
      }
    }
    self.columns = columns;
    Ok(())
  }

  fn keep_preview(&mut self, values: Vec<Value>) {
    if self.preview.len() >= self.config.preview_rows {
      return;
    }
    let size = row_size(&values, self.preview.is_empty());
    if self.preview_bytes + size <= self.config.inline_bytes {
      self.preview_bytes += size;
      self.preview.push(values);
    } else {
      // The preview stops at the first row that does not fit, so it stays a prefix.
      self.config.preview_rows = self.preview.len();
    }
  }

  /// One row of the current result set.
  pub fn row(&mut self, values: Vec<Value>) -> Result<(), DbError> {
    self.row_count += 1;
    if let Some(file) = &mut self.file {
      file.write_row(&values)?;
      self.keep_preview(values);
      return Ok(());
    }
    if self.config.dry_run && (self.config.out.is_some() || self.would_spill) {
      self.keep_preview(values);
      return Ok(());
    }
    let size = row_size(&values, self.inline.is_empty());
    if self.inline.len() < self.config.inline_rows
      && self.inline_bytes + size <= self.config.inline_bytes
    {
      self.inline_bytes += size;
      self.inline.push(values);
      return Ok(());
    }
    if self.config.dry_run {
      // Would spill: keep a preview, write nothing.
      self.would_spill = true;
      self.inline_bytes = 2;
      for row in std::mem::take(&mut self.inline) {
        self.keep_preview(row);
      }
      self.keep_preview(values);
      return Ok(());
    }
    // Past an inline threshold: everything goes to a spill file instead.
    let spill = spill_path(self.config.root.as_deref())?;
    let mut file = FileWriter::create(&spill, Format::Ndjson, &self.columns)?;
    let buffered = std::mem::take(&mut self.inline);
    for row in &buffered {
      file.write_row(row)?;
    }
    file.write_row(&values)?;
    self.file = Some(file);
    self.spilled = true;
    self.inline_bytes = 2;
    for row in buffered {
      self.keep_preview(row);
    }
    self.keep_preview(values);
    Ok(())
  }

  /// Commit the file (if any) and return what the call produced. With `out` and no result set
  /// at all, an empty file is still written.
  pub fn finish(mut self) -> Result<Collected, DbError> {
    if self.config.dry_run && (self.config.out.is_some() || self.would_spill) {
      return Ok(Collected::DryRunFile {
        columns: std::mem::take(&mut self.columns),
        row_count: self.row_count,
        preview: std::mem::take(&mut self.preview),
        would_write: self.config.out.clone(),
        would_spill: self.would_spill,
      });
    }
    if self.file.is_none() && !self.config.dry_run {
      if let Some((path, format)) = self.config.out.clone() {
        self.file = Some(FileWriter::create(&path, format, &self.columns)?);
      }
    }
    match self.file.take() {
      None => Ok(Collected::Inline {
        columns: std::mem::take(&mut self.columns),
        rows: std::mem::take(&mut self.inline),
      }),
      Some(file) => {
        let info = file.commit()?;
        Ok(Collected::File {
          columns: std::mem::take(&mut self.columns),
          row_count: self.row_count,
          preview: std::mem::take(&mut self.preview),
          file: info,
          spilled: self.spilled,
        })
      }
    }
  }
}

fn columns_json(columns: &[Column]) -> Value {
  Value::Array(
    columns
      .iter()
      .map(|c| json!({"name": c.name, "type": c.decl_type}))
      .collect(),
  )
}

impl Collected {
  /// The response document of `query` / `exec`.
  pub fn to_json(&self, stats: &RunStats) -> Value {
    match self {
      Collected::Inline { columns, rows } => json!({
        "columns": columns_json(columns),
        "rows": rows,
        "row_count": rows.len(),
        "rows_affected": stats.rows_affected,
        "last_insert_rowid": stats.last_insert_rowid,
        "spilled": false,
        "truncated": false,
        "truncated_by": null,
      }),
      Collected::DryRunFile {
        columns,
        row_count,
        preview,
        would_write,
        would_spill,
      } => json!({
        "columns": columns_json(columns),
        "row_count": row_count,
        "rows_affected": stats.rows_affected,
        "last_insert_rowid": stats.last_insert_rowid,
        "preview": preview,
        "file": null,
        "would_write": would_write.as_ref().map(|(path, format)| json!({
          "path": path.display().to_string(),
          "format": format.as_str(),
        })),
        "note": if *would_spill {
          json!("dry run: the full result would have spilled to a file; nothing was written")
        } else {
          json!("dry run: the out file was not written")
        },
        "spilled": false,
        "truncated": false,
        "truncated_by": null,
      }),
      Collected::File {
        columns,
        row_count,
        preview,
        file,
        spilled,
      } => json!({
        "columns": columns_json(columns),
        "row_count": row_count,
        "rows_affected": stats.rows_affected,
        "last_insert_rowid": stats.last_insert_rowid,
        "file": {
          "path": file.path.display().to_string(),
          "format": file.format.as_str(),
          "bytes": file.bytes,
          "rows": file.rows,
        },
        "preview": preview,
        "spilled": spilled,
        "overwrote": file.overwrote,
        "truncated": false,
        "truncated_by": null,
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn col(name: &str) -> Column {
    Column {
      name: name.to_string(),
      decl_type: None,
    }
  }

  #[test]
  fn csv_fields_follow_rfc_4180_and_keep_null_and_empty_apart() {
    assert_eq!(csv_field(&Value::Null), "");
    assert_eq!(csv_field(&json!("")), "\"\"");
    assert_eq!(csv_field(&json!("plain")), "plain");
    assert_eq!(csv_field(&json!("a,b")), "\"a,b\"");
    assert_eq!(csv_field(&json!("say \"hi\"")), "\"say \"\"hi\"\"\"");
    assert_eq!(csv_field(&json!("two\nlines")), "\"two\nlines\"");
    assert_eq!(csv_field(&json!("☃ snow")), "☃ snow");
    assert_eq!(csv_field(&json!(1.5)), "1.5");
    assert_eq!(csv_field(&json!(true)), "true");
    assert_eq!(
      csv_field(&json!({"$blob_base64": "AP8Q"})),
      "\"{\"\"$blob_base64\"\":\"\"AP8Q\"\"}\""
    );
  }

  #[test]
  fn duplicate_column_names_get_suffixes() {
    let keys = column_keys(&[col("a"), col("a"), col("b"), col("a")]);
    assert_eq!(keys, ["a", "a_2", "b", "a_3"]);
  }

  #[test]
  fn formats_from_names_and_extensions() {
    assert_eq!(Format::infer(Path::new("x.CSV")), Format::Csv);
    assert_eq!(Format::infer(Path::new("x.json")), Format::Json);
    assert_eq!(Format::infer(Path::new("x.jsonl")), Format::Ndjson);
    assert_eq!(Format::infer(Path::new("x.txt")), Format::Ndjson);
    assert_eq!(Format::infer(Path::new("x")), Format::Ndjson);
    assert_eq!(Format::parse("bogus"), None);
  }

  #[test]
  fn civil_dates() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    assert_eq!(civil_from_days(11_016), (2000, 2, 29));
  }

  #[test]
  fn uncommitted_file_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("r.ndjson");
    let mut w = FileWriter::create(&target, Format::Ndjson, &[col("x")]).unwrap();
    w.write_row(&[json!(1)]).unwrap();
    drop(w);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
  }

  #[test]
  fn json_file_is_one_array() {
    let dir = tempfile::tempdir().unwrap();
    for n in [0, 1, 3] {
      let target = dir.path().join(format!("r{n}.json"));
      let mut w = FileWriter::create(&target, Format::Json, &[col("x"), col("x")]).unwrap();
      for i in 0..n {
        w.write_row(&[json!(i), json!("s")]).unwrap();
      }
      let info = w.commit().unwrap();
      let doc: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
      assert_eq!(doc.as_array().unwrap().len(), n);
      assert_eq!(info.rows, n as u64);
      if n > 0 {
        assert_eq!(doc[0], json!({"x": 0, "x_2": "s"}));
      }
    }
  }
}
