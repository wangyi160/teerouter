use std::str;

use bytes::{Buf, BytesMut};
use tokio_util::codec::Decoder;

use crate::Error as SseError;

const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct SseCodec {
    past_bom: bool,
    max_bytes: usize,
}
impl Default for SseCodec {
    fn default() -> Self {
        Self::bounded(2 * 1024 * 1024)
    }
}

impl Decoder for SseCodec {
    type Item = SseEvent;
    type Error = SseError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<SseEvent>, SseError> {
        if !self.skip_bom(src) {
            return Ok(None);
        }
        while let Some(end) = block_end(src) {
            if end > self.max_bytes {
                return Err(SseError::InvalidResponse("SSE event exceeds size limit"));
            }
            let block = src.split_to(end);
            let pending = lines(&block)
                .map(|(line, _)| line)
                .take_while(|line| !line.is_empty())
                .try_fold(Pending::default(), Pending::apply)?;
            if let Some(event) = pending.dispatch() {
                return Ok(Some(event));
            }
        }
        if src.len() > self.max_bytes {
            return Err(SseError::InvalidResponse("SSE event exceeds size limit"));
        }
        Ok(None)
    }

    fn decode_eof(&mut self, _pending: &mut BytesMut) -> Result<Option<SseEvent>, SseError> {
        Ok(None)
    }
}

impl SseCodec {
    pub(crate) fn bounded(max_bytes: usize) -> Self {
        Self {
            past_bom: false,
            max_bytes,
        }
    }
    fn skip_bom(&mut self, src: &mut BytesMut) -> bool {
        if self.past_bom {
            return true;
        }
        if src.starts_with(BOM) {
            src.advance(BOM.len());
        } else if BOM.starts_with(src) {
            return false;
        }
        self.past_bom = true;
        true
    }
}

fn block_end(bytes: &[u8]) -> Option<usize> {
    lines(bytes)
        .find(|(line, _)| line.is_empty())
        .map(|(_, end)| end)
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = (&[u8], usize)> {
    let mut cursor: usize = 0;
    std::iter::from_fn(move || {
        let rest = &bytes[cursor..];
        let end = rest.iter().position(|byte| matches!(byte, b'\n' | b'\r'))?;
        cursor += end + terminator_len(&rest[end..]);
        Some((&rest[..end], cursor))
    })
}

fn terminator_len(terminated: &[u8]) -> usize {
    match terminated {
        [b'\r', b'\n', ..] => 2,
        _ => 1,
    }
}

#[derive(Default)]
struct Pending {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    retry: Option<u64>,
}

impl Pending {
    fn apply(self, line: &[u8]) -> Result<Self, SseError> {
        let (name, value) = split_field(line);
        Ok(match name {
            b"event" => Self {
                event: Some(str::from_utf8(value)?.to_owned()),
                ..self
            },
            b"data" => Self {
                data: Some(append_data(self.data, str::from_utf8(value)?)),
                ..self
            },
            b"id" if !value.contains(&0) => Self {
                id: Some(str::from_utf8(value)?.to_owned()),
                ..self
            },
            b"retry" => Self {
                retry: parse_retry(value).or(self.retry),
                ..self
            },
            _ => self,
        })
    }

    fn dispatch(self) -> Option<SseEvent> {
        Some(SseEvent {
            event: self.event,
            data: self.data?,
            id: self.id,
            retry: self.retry,
        })
    }
}

fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return (line, &[]);
    };
    let value = &line[colon + 1..];
    (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
}

fn append_data(buffer: Option<String>, line: &str) -> String {
    match buffer {
        Some(existing) => format!("{existing}\n{line}"),
        None => line.to_owned(),
    }
}

fn parse_retry(value: &[u8]) -> Option<u64> {
    if !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::lf("\n", 1)]
    #[case::crlf("\r\n", 1)]
    #[case::cr("\r", 1)]
    #[case::uneven("\r\n", 7)]
    #[case::whole("\n", 1024)]
    fn decodes_bom_multiline_unicode_and_comments_across_network_boundaries(
        #[case] separator: &str,
        #[case] width: usize,
    ) {
        let wire = "\u{feff}: comment\nevent: message\ndata: 你好\ndata: world\nid: r\nretry: 42\n\ndata: [DONE]\n\n"
            .replace('\n', separator);
        let mut codec = SseCodec::default();
        let mut buffer = BytesMut::new();
        let mut events = Vec::new();
        for chunk in wire.as_bytes().chunks(width) {
            buffer.extend_from_slice(chunk);
            while let Some(event) = codec.decode(&mut buffer).unwrap() {
                events.push(event);
            }
        }
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("message".into()),
                    data: "你好\nworld".into(),
                    id: Some("r".into()),
                    retry: Some(42),
                },
                SseEvent {
                    data: "[DONE]".into(),
                    ..SseEvent::default()
                }
            ]
        );
    }

    #[rstest]
    fn discards_unterminated_event_at_eof() {
        let mut codec = SseCodec::default();
        let mut buffer = BytesMut::from(&b"data: incomplete\n"[..]);
        assert!(codec.decode(&mut buffer).unwrap().is_none());
        assert!(codec.decode_eof(&mut buffer).unwrap().is_none());
    }

    #[rstest]
    fn rejects_invalid_utf8() {
        let mut codec = SseCodec::default();
        let mut buffer = BytesMut::from(&b"data: \xff\n\n"[..]);
        assert!(matches!(codec.decode(&mut buffer), Err(SseError::Utf8(_))));
    }
}
