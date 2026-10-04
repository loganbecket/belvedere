//! Splitting a Thunderbird mbox file into raw messages, streaming.
//!
//! Thunderbird writes `From - <date>` on its own line before every
//! message (the mboxrd-ish variant with a literal `-` instead of a sender).
//! Messages can be tens of megabytes, files can be gigabytes, so this
//! reads sequentially from a given offset and hands back one message at a
//! time without loading the file.

use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

/// One raw message and where it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawMessage {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// Whether a line is a Thunderbird message separator.
pub fn is_separator(line: &[u8]) -> bool {
    line.starts_with(b"From - ")
}

/// Iterates messages in `reader` starting at `offset`. The reader is
/// positioned at `offset` first; `offset` must be a message boundary (0 or
/// an offset previously returned as a message start plus its length).
pub struct Messages<R: Read + Seek> {
    reader: BufReader<R>,
    pos: u64,
    /// A separator line already read, belonging to the next message.
    pending: Option<(u64, Vec<u8>)>,
    done: bool,
}

impl<R: Read + Seek> Messages<R> {
    pub fn from_offset(mut inner: R, offset: u64) -> io::Result<Self> {
        inner.seek(SeekFrom::Start(offset))?;
        Ok(Messages {
            reader: BufReader::with_capacity(1 << 16, inner),
            pos: offset,
            pending: None,
            done: false,
        })
    }

    /// Where the next message would start (the end of what was read).
    pub fn position(&self) -> u64 {
        match &self.pending {
            Some((at, _)) => *at,
            None => self.pos,
        }
    }

    fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let n = self.reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        self.pos += n as u64;
        Ok(Some(line))
    }
}

impl<R: Read + Seek> Iterator for Messages<R> {
    type Item = io::Result<RawMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Find the start: a pending separator, or scan forward to one.
        let (start, first) = match self.pending.take() {
            Some(p) => p,
            None => loop {
                let at = self.pos;
                match self.read_line() {
                    Ok(Some(line)) if is_separator(&line) => break (at, line),
                    Ok(Some(_)) => continue, // junk before the first separator
                    Ok(None) => {
                        self.done = true;
                        return None;
                    }
                    Err(e) => return Some(Err(e)),
                }
            },
        };
        let mut bytes = first;
        loop {
            let at = self.pos;
            match self.read_line() {
                Ok(Some(line)) => {
                    if is_separator(&line) {
                        self.pending = Some((at, line));
                        break;
                    }
                    bytes.extend_from_slice(&line);
                }
                Ok(None) => {
                    self.done = true;
                    break;
                }
                Err(e) => return Some(Err(e)),
            }
        }
        Some(Ok(RawMessage {
            offset: start,
            bytes,
        }))
    }
}

/// The message body after Thunderbird's `From - …` line: what a mail
/// parser should see.
pub fn strip_separator(raw: &[u8]) -> &[u8] {
    if is_separator(raw) {
        match raw.iter().position(|&b| b == b'\n') {
            Some(i) => &raw[i + 1..],
            None => &[],
        }
    } else {
        raw
    }
}

/// Thunderbird's own status flags, from the `X-Mozilla-Status` header.
/// Bit 0x0008 means the message was deleted and awaits compaction.
pub fn mozilla_status(raw: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(&raw[..raw.len().min(4096)]);
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("X-Mozilla-Status: ") {
            return u32::from_str_radix(v.trim(), 16).ok();
        }
    }
    None
}

pub const STATUS_EXPUNGED: u32 = 0x0008;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn mbox(messages: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            out.extend_from_slice(format!("From - Mon Oct 2{} 09:00:00 2026\n", i).as_bytes());
            out.extend_from_slice(m.as_bytes());
        }
        out
    }

    #[test]
    fn splits_messages_and_tracks_offsets() {
        let data = mbox(&[
            "Subject: one\n\nbody one\n",
            "Subject: two\n\nbody two\nFrom the start, not a separator\n",
            "Subject: three\n\nbody three\n",
        ]);
        let msgs: Vec<RawMessage> = Messages::from_offset(Cursor::new(&data), 0)
            .unwrap()
            .map(|m| m.unwrap())
            .collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].offset, 0);
        assert_eq!(msgs[1].offset, msgs[0].bytes.len() as u64);
        assert_eq!(
            msgs[2].offset,
            (msgs[0].bytes.len() + msgs[1].bytes.len()) as u64
        );
        assert!(String::from_utf8_lossy(&msgs[1].bytes).contains("From the start"));
        let total: usize = msgs.iter().map(|m| m.bytes.len()).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn resumes_from_an_offset() {
        let data = mbox(&["Subject: one\n\nA\n", "Subject: two\n\nB\n"]);
        let mut it = Messages::from_offset(Cursor::new(&data), 0).unwrap();
        let first = it.next().unwrap().unwrap();
        let resume_at = it.position();
        assert_eq!(resume_at, first.bytes.len() as u64);
        let rest: Vec<_> = Messages::from_offset(Cursor::new(&data), resume_at)
            .unwrap()
            .map(|m| m.unwrap())
            .collect();
        assert_eq!(rest.len(), 1);
        assert!(String::from_utf8_lossy(&rest[0].bytes).contains("Subject: two"));
        // Nothing after the end.
        assert_eq!(
            Messages::from_offset(Cursor::new(&data), data.len() as u64)
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn junk_before_the_first_separator_is_skipped() {
        let mut data = b"garbage\nmore garbage\n".to_vec();
        data.extend(mbox(&["Subject: one\n\nA\n"]));
        let msgs: Vec<_> = Messages::from_offset(Cursor::new(&data), 0)
            .unwrap()
            .map(|m| m.unwrap())
            .collect();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].offset, 21);
    }

    #[test]
    fn separator_strip_and_status() {
        let raw = b"From - Mon Oct 20 09:00:00 2026\nX-Mozilla-Status: 0009\nSubject: x\n\nbody\n";
        assert!(strip_separator(raw).starts_with(b"X-Mozilla-Status"));
        assert_eq!(mozilla_status(raw), Some(9));
        assert_ne!(mozilla_status(raw).unwrap() & STATUS_EXPUNGED, 0);
        assert_eq!(mozilla_status(b"Subject: none\n\n"), None);
    }
}
