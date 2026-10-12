//! Checksummed, append-only output frames with stable sequence cursors.

use crate::contract::{
    MAX_PAGE_BYTES, OUTPUT_CHUNK_OVERHEAD_BYTES, OutputChunkV1, OutputPageV1, OutputStreamV1,
};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: [u8; 4] = *b"AXO1";
pub const MAX_FRAME_BYTES: usize = 4096;
const HEADER_BYTES: usize = 4 + 4 + 8 + 1 + 32;

pub struct Spool {
    file: File,
    offsets: Vec<u64>,
    next_sequence: u64,
    payload_bytes: u64,
    max_payload_bytes: u64,
    truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendResult {
    pub next_sequence: u64,
    pub truncated: bool,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_frame(file: &mut File) -> io::Result<Option<OutputChunkV1>> {
    let mut header = [0u8; HEADER_BYTES];
    let read = file.read(&mut header[..1])?;
    if read == 0 {
        return Ok(None);
    }
    file.read_exact(&mut header[1..])?;
    if header[..4] != MAGIC {
        return Err(invalid("invalid output frame magic"));
    }
    let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(invalid("oversized output frame"));
    }
    let sequence = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let stream = match header[16] {
        0 => OutputStreamV1::Stdout,
        1 => OutputStreamV1::Stderr,
        _ => return Err(invalid("invalid output stream")),
    };
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    let mut hashed = Vec::with_capacity(13 + bytes.len());
    hashed.extend_from_slice(&header[4..17]);
    hashed.extend_from_slice(&bytes);
    if blake3::hash(&hashed).as_bytes() != &header[17..49] {
        return Err(invalid("output frame checksum mismatch"));
    }
    Ok(Some(OutputChunkV1 {
        sequence,
        stream,
        bytes,
    }))
}

fn write_frame(file: &mut File, chunk: &OutputChunkV1) -> io::Result<()> {
    let len = u32::try_from(chunk.bytes.len()).map_err(|_| invalid("frame too large"))?;
    let mut header = [0u8; HEADER_BYTES];
    header[..4].copy_from_slice(&MAGIC);
    header[4..8].copy_from_slice(&len.to_le_bytes());
    header[8..16].copy_from_slice(&chunk.sequence.to_le_bytes());
    header[16] = match chunk.stream {
        OutputStreamV1::Stdout => 0,
        OutputStreamV1::Stderr => 1,
    };
    let mut hashed = Vec::with_capacity(13 + chunk.bytes.len());
    hashed.extend_from_slice(&header[4..17]);
    hashed.extend_from_slice(&chunk.bytes);
    header[17..49].copy_from_slice(blake3::hash(&hashed).as_bytes());
    file.write_all(&header)?;
    file.write_all(&chunk.bytes)?;
    file.sync_data()?;
    Ok(())
}

impl Spool {
    /// Reopen with the cursor last durably committed in job state. Any complete
    /// or torn frames after that cursor were never published and are removed.
    pub fn open(path: &Path, committed_cursor: u64, max_payload_bytes: u64) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        let mut payload_bytes = 0;
        let mut offsets = Vec::new();
        for expected in 0..committed_cursor {
            offsets.push(file.stream_position()?);
            let frame = read_frame(&mut file)?
                .ok_or_else(|| invalid("committed cursor has missing output frame"))?;
            if frame.sequence != expected {
                return Err(invalid("output sequence mismatch"));
            }
            payload_bytes += frame.bytes.len() as u64;
        }
        let end = file.stream_position()?;
        file.set_len(end)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            offsets,
            next_sequence: committed_cursor,
            payload_bytes,
            max_payload_bytes,
            truncated: false,
        })
    }

    pub fn append(&mut self, stream: OutputStreamV1, bytes: &[u8]) -> io::Result<AppendResult> {
        let available = self.max_payload_bytes.saturating_sub(self.payload_bytes);
        let stored = bytes.len().min(available as usize);
        // A concurrent output page may have moved this file handle to any
        // retained cursor. Appends must never overwrite previously committed
        // frames.
        self.file.seek(SeekFrom::End(0))?;
        for part in bytes[..stored].chunks(MAX_FRAME_BYTES) {
            let chunk = OutputChunkV1 {
                sequence: self.next_sequence,
                stream,
                bytes: part.to_vec(),
            };
            let offset = self.file.stream_position()?;
            write_frame(&mut self.file, &chunk)?;
            self.offsets.push(offset);
            self.next_sequence += 1;
            self.payload_bytes += part.len() as u64;
        }
        self.truncated |= stored < bytes.len();
        Ok(AppendResult {
            next_sequence: self.next_sequence,
            truncated: self.truncated,
        })
    }

    pub fn restore_truncated(&mut self, truncated: bool) {
        self.truncated = truncated;
    }

    pub fn page(&mut self, cursor: u64, max_bytes: u32, eof: bool) -> io::Result<OutputPageV1> {
        if !(MAX_FRAME_BYTES as u32..=MAX_PAGE_BYTES).contains(&max_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output page size outside v1 bounds",
            ));
        }
        if cursor > self.next_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "future output cursor",
            ));
        }
        let offset = self
            .offsets
            .get(cursor as usize)
            .copied()
            .unwrap_or(self.file.metadata()?.len());
        self.file.seek(SeekFrom::Start(offset))?;
        let mut expected = cursor;
        let mut chunks = Vec::new();
        let mut total = 0usize;
        while expected < self.next_sequence {
            let offset = self.file.stream_position()?;
            let frame =
                read_frame(&mut self.file)?.ok_or_else(|| invalid("committed frame missing"))?;
            if frame.sequence != expected {
                return Err(invalid("output sequence mismatch"));
            }
            // Charging each chunk's encoding overhead bounds the encoded page;
            // the first chunk always fits so a reader always progresses.
            let cost = frame.bytes.len() + OUTPUT_CHUNK_OVERHEAD_BYTES;
            if !chunks.is_empty() && total + cost > max_bytes as usize {
                self.file.seek(SeekFrom::Start(offset))?;
                break;
            }
            total += cost;
            expected += 1;
            chunks.push(frame);
        }
        Ok(OutputPageV1 {
            chunks,
            next_cursor: expected,
            truncated: self.truncated,
            eof: eof && expected == self.next_sequence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_cursor_survives_reopen_and_uncommitted_tail_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut spool = Spool::open(&path, 0, 16).unwrap();
        assert_eq!(
            spool
                .append(OutputStreamV1::Stdout, b"hello")
                .unwrap()
                .next_sequence,
            1
        );
        spool
            .append(OutputStreamV1::Stderr, b"uncommitted")
            .unwrap();
        drop(spool);
        let mut reopened = Spool::open(&path, 1, 16).unwrap();
        let page = reopened.page(0, 4096, true).unwrap();
        assert_eq!(page.chunks.len(), 1);
        assert_eq!(page.chunks[0].bytes, b"hello");
        assert_eq!(page.next_cursor, 1);
        assert!(page.eof);
    }

    #[test]
    fn cap_marks_truncation_and_keeps_drained_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let mut spool = Spool::open(&temp.path().join("output.log"), 0, 3).unwrap();
        let result = spool.append(OutputStreamV1::Stdout, b"abcdef").unwrap();
        assert!(result.truncated);
        let page = spool.page(0, 4096, false).unwrap();
        assert_eq!(page.chunks[0].bytes, b"abc");
        assert!(page.truncated);
    }

    #[test]
    fn reading_a_page_does_not_move_the_append_position() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.log");
        let mut spool = Spool::open(&path, 0, 1024).unwrap();
        spool.append(OutputStreamV1::Stdout, b"first").unwrap();
        assert_eq!(spool.page(0, 4096, false).unwrap().next_cursor, 1);
        spool.append(OutputStreamV1::Stderr, b"second").unwrap();
        let page = spool.page(0, 4096, true).unwrap();
        assert_eq!(page.chunks.len(), 2);
        assert_eq!(page.chunks[0].bytes, b"first");
        assert_eq!(page.chunks[1].bytes, b"second");
    }

    #[test]
    fn pages_charge_chunk_overhead_but_always_progress() {
        let temp = tempfile::tempdir().unwrap();
        let mut spool = Spool::open(&temp.path().join("output.log"), 0, 1 << 20).unwrap();
        for _ in 0..200 {
            spool.append(OutputStreamV1::Stdout, b"x").unwrap();
        }
        let page = spool.page(0, 4096, false).unwrap();
        assert_eq!(page.chunks.len(), 4096 / (1 + OUTPUT_CHUNK_OVERHEAD_BYTES));
        let mut full = Spool::open(&temp.path().join("full.log"), 0, 1 << 20).unwrap();
        full.append(OutputStreamV1::Stdout, &[7; MAX_FRAME_BYTES])
            .unwrap();
        let page = full.page(0, MAX_FRAME_BYTES as u32, false).unwrap();
        assert_eq!(page.chunks.len(), 1);
        assert_eq!(page.next_cursor, 1);
    }
}
