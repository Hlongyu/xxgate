//! Bounded observation of one opaque protocol header; never use it for routing.
use base64::{Engine, engine::general_purpose::STANDARD};
use http::HeaderMap;
use serde::{Deserialize, Serialize};

const NAME: &str = "x-codex-turn-state";
const MAX_BYTES: usize = 64 * 1024;
const MAX_VALUES: usize = 8;

/// An explicit observation distinguishes an absent header from an old record
/// where no observation was made. Values remain opaque and are never decoded.
#[derive(Clone, Serialize, Deserialize)]
pub struct TurnStateHeader {
    pub present: bool,
    pub total_values: usize,
    pub omitted_values: usize,
    pub values: Vec<TurnStateValue>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnStateValue {
    /// UTF-8 text as received, or Base64 for non-UTF-8 header bytes.
    pub value: String,
    pub encoding: String,
    pub bytes: usize,
    pub truncated: bool,
}

// Debug output must not duplicate opaque state into ordinary application logs.
impl std::fmt::Debug for TurnStateHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnStateHeader")
            .field("present", &self.present)
            .field("total_values", &self.total_values)
            .finish_non_exhaustive()
    }
}

impl TurnStateHeader {
    pub fn capture(headers: &HeaderMap) -> Self {
        let total_values = headers.get_all(NAME).iter().count();
        let mut budget = MAX_BYTES;
        let mut values = Vec::new();
        for raw in headers.get_all(NAME).iter().take(MAX_VALUES) {
            if budget == 0 {
                break;
            }
            let bytes = raw.as_bytes();
            let mut length = bytes.len().min(budget);
            let (value, encoding) = match std::str::from_utf8(bytes) {
                Ok(text) => {
                    while !text.is_char_boundary(length) {
                        length -= 1;
                    }
                    (text[..length].to_owned(), "utf8")
                }
                Err(_) => (STANDARD.encode(&bytes[..length]), "base64"),
            };
            budget -= length;
            values.push(TurnStateValue {
                value,
                encoding: encoding.into(),
                bytes: bytes.len(),
                truncated: length < bytes.len(),
            });
        }
        Self {
            present: total_values != 0,
            total_values,
            omitted_values: total_values - values.len(),
            values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn absent_empty_multiple_and_opaque_bytes_are_distinct() {
        let mut headers = HeaderMap::new();
        let absent = TurnStateHeader::capture(&headers);
        assert!(!absent.present);
        assert!(absent.values.is_empty());
        headers.append(NAME, HeaderValue::from_static(""));
        headers.append(NAME, HeaderValue::from_static("  OPAQUE_STATE+/=  "));
        headers.append(NAME, HeaderValue::from_bytes(&[0xff, 0x80]).unwrap());
        let capture = TurnStateHeader::capture(&headers);
        assert!(capture.present);
        assert_eq!(capture.total_values, 3);
        assert_eq!(capture.values[0].value, "");
        assert_eq!(capture.values[1].value, "  OPAQUE_STATE+/=  ");
        assert_eq!(capture.values[2].encoding, "base64");
        assert_eq!(
            STANDARD.decode(&capture.values[2].value).unwrap(),
            [0xff, 0x80]
        );
        assert!(!format!("{capture:?}").contains("OPAQUE_STATE"));
    }

    #[test]
    fn capture_bounds_total_bytes_and_count_without_changing_headers() {
        let mut headers = HeaderMap::new();
        let large = "界".repeat(MAX_BYTES);
        headers.append(NAME, HeaderValue::from_bytes(large.as_bytes()).unwrap());
        for _ in 0..12 {
            headers.append(NAME, HeaderValue::from_static("opaque"));
        }
        let capture = TurnStateHeader::capture(&headers);
        assert!(capture.values[0].truncated);
        assert_eq!(capture.values[0].bytes, large.len());
        assert!(capture.values.iter().map(|v| v.value.len()).sum::<usize>() <= MAX_BYTES);
        assert!(capture.omitted_values > 0);
        assert_eq!(headers[NAME].as_bytes(), large.as_bytes());
        headers.remove(NAME);
        for _ in 0..12 {
            headers.append(NAME, HeaderValue::from_static("opaque"));
        }
        let capture = TurnStateHeader::capture(&headers);
        assert_eq!(capture.values.len(), MAX_VALUES);
        assert_eq!(capture.omitted_values, 4);
    }
}
