use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use serde::de::IgnoredAny;
use serde::Deserialize;

const INPUT_BYTES: usize = 8192;
const KEY_BYTES: usize = 12 * 6 + 2;
const SHA_CHARS: usize = 64;
const SHA_BYTES: usize = SHA_CHARS * 6 + 2;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RequestBody {
    pub bypass_rules: bool,
    pub sha: Option<String>,
}

#[derive(Clone, Copy)]
enum Field {
    Bypass,
    Sha,
}

struct Projection {
    body: RequestBody,
    started: bool,
    object: bool,
    depth: usize,
    in_string: bool,
    escaped: bool,
    key_expected: bool,
    value_expected: bool,
    field: Option<Field>,
    capture_key: bool,
    capture_sha: bool,
    token: Vec<u8>,
}

impl Default for Projection {
    fn default() -> Self {
        Self {
            body: RequestBody::default(),
            started: false,
            object: false,
            depth: 0,
            in_string: false,
            escaped: false,
            key_expected: false,
            value_expected: false,
            field: None,
            capture_key: false,
            capture_sha: false,
            token: Vec::with_capacity(SHA_BYTES),
        }
    }
}

impl Projection {
    fn feed(&mut self, byte: u8) {
        if self.in_string {
            if self.capture_key || self.capture_sha {
                let limit = if self.capture_key {
                    KEY_BYTES
                } else {
                    SHA_BYTES
                };
                if self.token.len() == limit {
                    self.capture_key = false;
                    self.capture_sha = false;
                    self.token.clear();
                } else {
                    self.token.push(byte);
                }
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
                if self.capture_key {
                    self.field = match sonic_rs::from_slice::<String>(&self.token).ok().as_deref() {
                        Some("bypass_rules") => Some(Field::Bypass),
                        Some("sha") => Some(Field::Sha),
                        _ => None,
                    };
                } else if self.capture_sha {
                    self.body.sha = sonic_rs::from_slice::<String>(&self.token)
                        .ok()
                        .filter(|value| value.len() <= SHA_CHARS);
                }
                self.capture_key = false;
                self.capture_sha = false;
                self.token.clear();
            }
            return;
        }
        if byte.is_ascii_whitespace() {
            return;
        }
        if !self.started {
            self.started = true;
            self.object = byte == b'{';
        }
        let root = self.object && self.depth == 1;
        let value = root && self.value_expected;
        if value {
            match self.field {
                Some(Field::Bypass) => self.body.bypass_rules = byte == b't',
                Some(Field::Sha) => self.body.sha = None,
                None => {}
            }
            self.value_expected = false;
        }
        match byte {
            b'"' => {
                self.in_string = true;
                self.capture_key = root && self.key_expected;
                self.capture_sha = value && matches!(self.field, Some(Field::Sha));
                if self.capture_key {
                    self.field = None;
                    self.key_expected = false;
                }
                if self.capture_key || self.capture_sha {
                    self.token.push(byte);
                }
            }
            b'{' | b'[' => {
                self.depth += 1;
                if self.object && self.depth == 1 {
                    self.key_expected = true;
                }
            }
            b'}' | b']' => self.depth = self.depth.saturating_sub(1),
            b',' if root => self.key_expected = true,
            b':' if root => self.value_expected = true,
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub enum RequestBodyTransport {
    Identity,
    CurlData,
}

struct TransportRead<R> {
    source: R,
    transport: RequestBodyTransport,
}

impl<R: Read> Read for TransportRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            let count = self.source.read(output)?;
            if count == 0 || matches!(self.transport, RequestBodyTransport::Identity) {
                return Ok(count);
            }
            let mut retained = 0;
            for index in 0..count {
                if !matches!(output[index], b'\r' | b'\n' | 0) {
                    output[retained] = output[index];
                    retained += 1;
                }
            }
            if retained > 0 {
                return Ok(retained);
            }
        }
    }
}

struct Utf8Read<R> {
    source: R,
    tail: [u8; 4],
    tail_len: usize,
}

impl<R: Read> Read for Utf8Read<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let count = self.source.read(output)?;
        if count == 0 {
            return if self.tail_len == 0 {
                Ok(0)
            } else {
                Err(invalid_utf8())
            };
        }
        let mut start = 0;
        if self.tail_len > 0 {
            while start < count && self.tail_len < self.tail.len() {
                self.tail[self.tail_len] = output[start];
                self.tail_len += 1;
                start += 1;
                match std::str::from_utf8(&self.tail[..self.tail_len]) {
                    Ok(_) => {
                        self.tail_len = 0;
                        break;
                    }
                    Err(error) if error.error_len().is_none() => {}
                    Err(_) => return Err(invalid_utf8()),
                }
            }
            if self.tail_len > 0 {
                return Ok(count);
            }
        }
        match std::str::from_utf8(&output[start..count]) {
            Ok(_) => {}
            Err(error) if error.error_len().is_none() => {
                let remaining = &output[start + error.valid_up_to()..count];
                self.tail[..remaining.len()].copy_from_slice(remaining);
                self.tail_len = remaining.len();
            }
            Err(_) => return Err(invalid_utf8()),
        }
        Ok(count)
    }
}

fn invalid_utf8() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid request JSON UTF-8")
}

struct ProjectedRead<R> {
    source: BufReader<Utf8Read<R>>,
    projection: Projection,
    replacement: &'static [u8],
}

impl<R: Read> ProjectedRead<R> {
    fn new(source: R) -> Self {
        Self {
            source: BufReader::with_capacity(
                INPUT_BYTES,
                Utf8Read {
                    source,
                    tail: [0; 4],
                    tail_len: 0,
                },
            ),
            projection: Projection::default(),
            replacement: &[],
        }
    }

    fn original(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0];
        if self.source.read(&mut byte)? == 0 {
            return Ok(None);
        }
        self.projection.feed(byte[0]);
        Ok(Some(byte[0]))
    }

    fn nonfinite(&mut self, suffix: &[u8]) -> io::Result<()> {
        for expected in suffix {
            if self.original()? != Some(*expected) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid request JSON literal",
                ));
            }
        }
        self.replacement = b"ull";
        Ok(())
    }
}

impl<R: Read> Read for ProjectedRead<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some((&first, rest)) = self.replacement.split_first() {
            output[0] = first;
            self.replacement = rest;
            return Ok(1);
        }
        let in_string = self.projection.in_string;
        let Some(byte) = self.original()? else {
            return Ok(0);
        };
        if !in_string {
            let suffix = match byte {
                b'N' => Some(&b"aN"[..]),
                b'I' => Some(&b"nfinity"[..]),
                b'-' if self.source.fill_buf()?.first() == Some(&b'I') => Some(&b"Infinity"[..]),
                _ => None,
            };
            if let Some(suffix) = suffix {
                self.nonfinite(suffix)?;
                output[0] = b'n';
                return Ok(1);
            }
        }
        output[0] = byte;
        Ok(1)
    }
}

pub fn project_request_body(source: impl Read) -> Result<Option<RequestBody>, serde_json::Error> {
    let mut reader = ProjectedRead::new(source);
    let mut decoder = serde_json::Deserializer::from_reader(&mut reader);
    IgnoredAny::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(reader.projection.object.then_some(reader.projection.body))
}

pub fn read_request_body(path: &Path, transport: RequestBodyTransport) -> io::Result<Option<RequestBody>> {
    #[cfg(not(unix))]
    if !path.metadata()?.is_file() {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let source = options.open(path)?;
    if !source.metadata()?.is_file() {
        return Ok(None);
    }
    Ok(project_request_body(TransportRead { source, transport }).ok().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortReads<'a> {
        input: &'a [u8],
        width: usize,
    }

    impl Read for ShortReads<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            assert!(output.len() <= INPUT_BYTES);
            let count = self.width.min(output.len()).min(self.input.len());
            output[..count].copy_from_slice(&self.input[..count]);
            self.input = &self.input[count..];
            Ok(count)
        }
    }

    #[test]
    fn request_projection_curl_data_matches_filtered_bytes_across_chunk_boundaries() {
        let input = b"\r\n\0{\"by\r\n\0pass_rules\":tr\r\n\0ue,\"sha\":\"aaaaaaaaaaaaaaaaaaaa\0aaaaaaaaaaaaaaaaaaaa\",\"ignored\":\"\xc3\0\xa9\xf0\r\n\x9f\x99\x82\"}\r\n\0";
        let expected: Vec<_> = input.iter().copied().filter(|byte| !matches!(*byte, b'\r' | b'\n' | 0)).collect();
        let expected = project_request_body(expected.as_slice()).unwrap();
        assert!(expected.as_ref().unwrap().bypass_rules);
        for width in 1..=17 {
            let source = ShortReads { input, width };
            let actual = project_request_body(TransportRead { source, transport: RequestBodyTransport::CurlData }).unwrap();
            assert_eq!(actual, expected);
            let source = ShortReads { input, width };
            assert!(project_request_body(TransportRead { source, transport: RequestBodyTransport::Identity }).is_err());
        }
        for width in 1..=17 {
            let source = ShortReads { input: b"\r\n\0\r\n\0", width };
            assert!(project_request_body(TransportRead { source, transport: RequestBodyTransport::CurlData }).is_err());
        }
    }

    #[test]
    fn request_projection_preserves_duplicates_extensions_and_utf8_boundaries() {
        let input = br#"{"bypass_rules":false,"\u0062ypass_rules":true,"sha":"wrong","\u0073ha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","nested":{"bypass_rules":false,"value":[NaN,Infinity,-Infinity,"\ud800"]},"text":"\"escaped\""}"#;
        for width in 1..=17 {
            let actual = project_request_body(ShortReads { input, width })
                .unwrap()
                .unwrap();
            assert!(actual.bypass_rules);
            assert_eq!(
                actual.sha.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
            let actual = project_request_body(ShortReads {
                input: "{\"ignored\":\"é🦀\",\"bypass_rules\":true}".as_bytes(),
                width,
            })
            .unwrap()
            .unwrap();
            assert!(actual.bypass_rules);
        }
    }

    #[test]
    fn request_projection_rejects_partial_or_invalid_documents() {
        for input in [
            &b"{\"bypass_rules\":true"[..],
            &b"{\"bypass_rules\":true} trailing"[..],
            &b"{\"bypass_rules\":true,}"[..],
            &b"{\"bypass_rules\":true,\"ignored\":\"\xff\"}"[..],
            &b"{\"bypass_rules\":true,\"ignored\":\"\xf0\x9f\"}"[..],
            &b"{\"bypass_rules\":true,\"ignored\":Infinityx}"[..],
        ] {
            for width in 1..=7 {
                assert!(project_request_body(ShortReads { input, width }).is_err());
            }
        }
        for input in [
            "[]",
            "true",
            "NaN",
            "Infinity",
            "-Infinity",
            "null",
            "\"text\"",
        ] {
            assert_eq!(project_request_body(input.as_bytes()).unwrap(), None);
        }
    }

    #[test]
    fn request_projection_keeps_oversized_sha_recognized_and_validates_deep_values() {
        let mut input = format!(
            "{{\"bypass_rules\":true,\"sha\":\"{}\",\"ignored\":",
            "a".repeat(4096)
        );
        input.push_str(&"[".repeat(8192));
        input.push_str("0");
        input.push_str(&"]".repeat(8192));
        input.push('}');
        let actual = project_request_body(input.as_bytes()).unwrap().unwrap();
        assert!(actual.bypass_rules);
        assert_eq!(actual.sha, None);
        for value in [
            "1", "\"true\"", "null", "false", "{}", "[]", "NaN", "Infinity",
        ] {
            let input = format!("{{\"bypass_rules\":true,\"bypass_rules\":{value}}}");
            assert!(
                !project_request_body(input.as_bytes())
                    .unwrap()
                    .unwrap()
                    .bypass_rules
            );
        }
    }
}
