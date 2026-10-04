//! Reads what a GGUF model file says about itself without loading it:
//! the header and key/value metadata only, never the tensors.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// What we keep from a model file's metadata.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GgufInfo {
    /// `general.architecture`, e.g. "llama", "qwen3", "gemma3".
    pub architecture: String,
    /// `general.name`, or empty.
    pub name: String,
    /// `general.size_label`, e.g. "9B", or empty.
    pub size_label: String,
    /// `general.parameter_count`, if recorded.
    pub parameter_count: Option<u64>,
    /// Quantization name, e.g. "Q4_K_M", from the file type.
    pub quantization: String,
    /// `<arch>.context_length`, if recorded.
    pub context_length: Option<u64>,
    /// `tokenizer.chat_template`, or empty.
    pub chat_template: String,
    /// `general.type`: "model", "mmproj" (a vision projector), etc.
    pub kind: String,
}

impl GgufInfo {
    /// Whether this file is a helper (vision projector, adapter) rather
    /// than a model you can chat with.
    pub fn is_helper(&self) -> bool {
        self.kind == "mmproj"
            || self.kind == "adapter"
            || self.architecture == "clip"
            || self.architecture == "mmproj"
    }

    /// Whether the chat template knows how to present tools to the model.
    /// A template that never mentions tools cannot do tool calls.
    pub fn supports_tools(&self) -> bool {
        let t = &self.chat_template;
        t.contains("tools") || t.contains("tool_call")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GgufError {
    #[error("not a GGUF file")]
    BadMagic,
    #[error("unsupported GGUF version {0}")]
    Version(u32),
    #[error("unknown metadata value type {0}")]
    ValueType(u32),
    #[error("metadata is malformed: {0}")]
    Malformed(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A metadata value we care about. Arrays are kept only by length.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    Str(String),
    Array(usize),
}

impl Value {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Value::U64(v) => Some(*v),
            Value::I64(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Strings longer than this (the whole tokenizer vocabulary is an array of
/// strings, for instance) are skipped rather than read.
const MAX_STRING: u64 = 1 << 20;

pub fn read_info(path: &Path) -> Result<GgufInfo, GgufError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    read_info_from(&mut reader)
}

pub fn read_info_from<R: Read>(r: &mut R) -> Result<GgufInfo, GgufError> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        return Err(GgufError::BadMagic);
    }
    let version = read_u32(r)?;
    if !(2..=3).contains(&version) {
        return Err(GgufError::Version(version));
    }
    let _tensor_count = read_u64(r)?;
    let kv_count = read_u64(r)?;

    let mut kv: HashMap<String, Value> = HashMap::new();
    for _ in 0..kv_count {
        let key = read_string(r)?.ok_or(GgufError::Malformed("key too long"))?;
        let ty = read_u32(r)?;
        let value = read_value(r, ty)?;
        kv.insert(key, value);
    }

    let architecture = kv
        .get("general.architecture")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let get_str = |k: &str| {
        kv.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let file_type = kv.get("general.file_type").and_then(Value::as_u64);

    Ok(GgufInfo {
        name: get_str("general.name"),
        size_label: get_str("general.size_label"),
        parameter_count: kv.get("general.parameter_count").and_then(Value::as_u64),
        quantization: file_type.map(quantization_name).unwrap_or_default(),
        context_length: kv
            .get(&format!("{architecture}.context_length"))
            .and_then(Value::as_u64),
        chat_template: get_str("tokenizer.chat_template"),
        kind: get_str("general.type"),
        architecture,
    })
}

/// llama.cpp's `general.file_type` numbers.
pub fn quantization_name(file_type: u64) -> String {
    let name = match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4",
        other => return format!("type {other}"),
    };
    name.to_string()
}

/// Pulls a quantization name like `Q4_K_M` out of a file name, for files
/// whose metadata doesn't say.
pub fn quantization_from_name(file_name: &str) -> Option<String> {
    let upper = file_name.to_ascii_uppercase();
    let candidates = [
        "Q4_K_M", "Q4_K_S", "Q5_K_M", "Q5_K_S", "Q3_K_L", "Q3_K_M", "Q3_K_S", "Q6_K", "Q8_0",
        "Q4_0", "Q4_1", "Q5_0", "Q5_1", "Q2_K", "IQ4_XS", "IQ4_NL", "IQ3_M", "IQ3_S", "IQ2_M",
        "BF16", "F16", "F32",
    ];
    candidates
        .iter()
        .find(|q| upper.contains(*q))
        .map(|q| q.to_string())
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Reads a string, or skips it (returning `None`) if it is absurdly long.
fn read_string<R: Read>(r: &mut R) -> Result<Option<String>, GgufError> {
    let len = read_u64(r)?;
    if len > MAX_STRING {
        skip(r, len)?;
        return Ok(None);
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn scalar_size(ty: u32) -> Option<u64> {
    Some(match ty {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4..=6 => 4,
        10..=12 => 8,
        _ => return None,
    })
}

fn read_value<R: Read>(r: &mut R, ty: u32) -> Result<Value, GgufError> {
    Ok(match ty {
        0 => Value::U64(read_u8(r)? as u64),
        1 => Value::I64(read_u8(r)? as i8 as i64),
        2 => Value::U64(read_u16(r)? as u64),
        3 => Value::I64(read_u16(r)? as i16 as i64),
        4 => Value::U64(read_u32(r)? as u64),
        5 => Value::I64(read_u32(r)? as i32 as i64),
        6 => Value::F64(f32::from_le_bytes(read_u32(r)?.to_le_bytes()) as f64),
        7 => Value::Bool(read_u8(r)? != 0),
        8 => Value::Str(read_string(r)?.unwrap_or_default()),
        9 => {
            let elem_ty = read_u32(r)?;
            let count = read_u64(r)?;
            skip_array(r, elem_ty, count)?;
            Value::Array(count as usize)
        }
        10 => Value::U64(read_u64(r)?),
        11 => Value::I64(read_u64(r)? as i64),
        12 => Value::F64(f64::from_le_bytes(read_u64(r)?.to_le_bytes())),
        other => return Err(GgufError::ValueType(other)),
    })
}

/// Reads and discards `n` bytes. Sequential reads stay inside the
/// buffered reader's buffer; seeking would throw that buffer away on
/// every one of a vocabulary's hundred thousand entries.
fn skip<R: Read>(r: &mut R, n: u64) -> io::Result<()> {
    let copied = io::copy(&mut r.by_ref().take(n), &mut io::sink())?;
    if copied != n {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file ends inside metadata",
        ));
    }
    Ok(())
}

/// Arrays can hold a whole vocabulary; skip them without keeping them.
fn skip_array<R: Read>(r: &mut R, elem_ty: u32, count: u64) -> Result<(), GgufError> {
    if let Some(size) = scalar_size(elem_ty) {
        skip(r, size * count)?;
        return Ok(());
    }
    match elem_ty {
        8 => {
            for _ in 0..count {
                let len = read_u64(r)?;
                skip(r, len)?;
            }
            Ok(())
        }
        9 => {
            for _ in 0..count {
                let inner_ty = read_u32(r)?;
                let inner_count = read_u64(r)?;
                skip_array(r, inner_ty, inner_count)?;
            }
            Ok(())
        }
        other => Err(GgufError::ValueType(other)),
    }
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

/// Builds GGUF bytes with the given metadata, for tests and fixtures. Only
/// the header and metadata are written; there are no tensors.
#[cfg(any(test, feature = "fixtures"))]
pub mod fixture {
    /// A metadata value for the builder.
    pub enum V<'a> {
        U32(u32),
        U64(u64),
        Str(&'a str),
        StrArray(&'a [&'a str]),
        F32(f32),
    }

    pub fn gguf(kv: &[(&str, V<'_>)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes()); // tensors
        out.extend_from_slice(&(kv.len() as u64).to_le_bytes());
        for (key, value) in kv {
            put_str(&mut out, key);
            match value {
                V::U32(v) => {
                    out.extend_from_slice(&4u32.to_le_bytes());
                    out.extend_from_slice(&v.to_le_bytes());
                }
                V::U64(v) => {
                    out.extend_from_slice(&10u32.to_le_bytes());
                    out.extend_from_slice(&v.to_le_bytes());
                }
                V::F32(v) => {
                    out.extend_from_slice(&6u32.to_le_bytes());
                    out.extend_from_slice(&v.to_le_bytes());
                }
                V::Str(s) => {
                    out.extend_from_slice(&8u32.to_le_bytes());
                    put_str(&mut out, s);
                }
                V::StrArray(items) => {
                    out.extend_from_slice(&9u32.to_le_bytes());
                    out.extend_from_slice(&8u32.to_le_bytes());
                    out.extend_from_slice(&(items.len() as u64).to_le_bytes());
                    for s in *items {
                        put_str(&mut out, s);
                    }
                }
            }
        }
        out
    }

    fn put_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{gguf, V};
    use super::*;
    use std::io::Cursor;

    fn chat_model() -> Vec<u8> {
        gguf(&[
            ("general.architecture", V::Str("qwen3")),
            ("general.type", V::Str("model")),
            ("general.name", V::Str("Qwen3 9B")),
            ("general.size_label", V::Str("9B")),
            ("general.parameter_count", V::U64(9_000_000_000)),
            ("general.file_type", V::U32(15)),
            ("qwen3.context_length", V::U32(40_960)),
            ("qwen3.rope.freq_base", V::F32(1_000_000.0)),
            (
                "tokenizer.ggml.tokens",
                V::StrArray(&["<s>", "</s>", "hello", "world"]),
            ),
            (
                "tokenizer.chat_template",
                V::Str("{% if tools %}...{% endif %}{% for m in messages %}{{ m.content }}{% endfor %}"),
            ),
        ])
    }

    #[test]
    fn reads_the_fields_we_need_and_skips_the_vocabulary() {
        let info = read_info_from(&mut Cursor::new(chat_model())).unwrap();
        assert_eq!(info.architecture, "qwen3");
        assert_eq!(info.name, "Qwen3 9B");
        assert_eq!(info.size_label, "9B");
        assert_eq!(info.parameter_count, Some(9_000_000_000));
        assert_eq!(info.quantization, "Q4_K_M");
        assert_eq!(info.context_length, Some(40_960));
        assert!(info.chat_template.contains("tools"));
        assert!(info.supports_tools());
        assert!(!info.is_helper());
    }

    #[test]
    fn projector_files_are_helpers() {
        let bytes = gguf(&[
            ("general.architecture", V::Str("clip")),
            ("general.type", V::Str("mmproj")),
            ("general.file_type", V::U32(1)),
        ]);
        let info = read_info_from(&mut Cursor::new(bytes)).unwrap();
        assert!(info.is_helper());
        assert_eq!(info.quantization, "F16");
        assert!(!info.supports_tools());
    }

    #[test]
    fn template_without_tools_cannot_call_tools() {
        let bytes = gguf(&[
            ("general.architecture", V::Str("gemma3")),
            (
                "tokenizer.chat_template",
                V::Str("{% for m in messages %}{{ m.content }}{% endfor %}"),
            ),
        ]);
        let info = read_info_from(&mut Cursor::new(bytes)).unwrap();
        assert!(!info.supports_tools());
        assert_eq!(info.quantization, "");
    }

    #[test]
    fn rejects_non_gguf_and_odd_versions() {
        assert!(matches!(
            read_info_from(&mut Cursor::new(b"PK\x03\x04junk".to_vec())),
            Err(GgufError::BadMagic)
        ));
        let mut bytes = chat_model();
        bytes[4..8].copy_from_slice(&9u32.to_le_bytes());
        assert!(matches!(
            read_info_from(&mut Cursor::new(bytes)),
            Err(GgufError::Version(9))
        ));
        assert!(read_info_from(&mut Cursor::new(b"GGUF".to_vec())).is_err());
    }

    #[test]
    fn quantization_from_file_names() {
        assert_eq!(
            quantization_from_name("Qwen3.5-9B-Q4_K_M.gguf").as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quantization_from_name("gemma-3-1B-it-QAT-Q4_0.gguf").as_deref(),
            Some("Q4_0")
        );
        assert_eq!(
            quantization_from_name("mmproj-F32.gguf").as_deref(),
            Some("F32")
        );
        assert_eq!(quantization_from_name("model.gguf"), None);
    }
}
