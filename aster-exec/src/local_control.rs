//! Bounded local protocol between the unprivileged API and one job supervisor.

use crate::contract::{CancelReceiptV1, JobStatusV1, OutputPageV1};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequestV1 {
    Status,
    Output { cursor: u64, max_bytes: u32 },
    Cancel { grace_ms: Option<u32> },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ControlResponseV1 {
    Status(Box<JobStatusV1>),
    Output(OutputPageV1),
    Cancel(CancelReceiptV1),
    Error(crate::contract::ExecErrorV1),
}

pub fn call(path: &Path, request: &ControlRequestV1) -> io::Result<ControlResponseV1> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let bytes = serde_json::to_vec(request).map_err(io::Error::other)?;
    if bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local request too large",
        ));
    }
    stream.write_all(&bytes)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Vec::new();
    stream
        .take(crate::contract::MAX_LOCAL_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut response)?;
    if response.len() > crate::contract::MAX_LOCAL_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "local response too large",
        ));
    }
    serde_json::from_slice(&response).map_err(|error| {
        io::Error::new(
            if error.is_eof() {
                io::ErrorKind::UnexpectedEof
            } else {
                io::ErrorKind::InvalidData
            },
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maximum_output_page_crosses_local_socket() {
        use crate::contract::{MAX_PAGE_BYTES, OutputChunkV1, OutputStreamV1};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("control.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            let page = OutputPageV1 {
                chunks: (0..256)
                    .map(|sequence| OutputChunkV1 {
                        sequence,
                        stream: OutputStreamV1::Stdout,
                        bytes: vec![255; 4096],
                    })
                    .collect(),
                next_cursor: 256,
                truncated: false,
                eof: true,
            };
            stream
                .write_all(&serde_json::to_vec(&ControlResponseV1::Output(page)).unwrap())
                .unwrap();
        });
        let response = call(
            &path,
            &ControlRequestV1::Output {
                cursor: 0,
                max_bytes: MAX_PAGE_BYTES,
            },
        )
        .unwrap();
        match response {
            ControlResponseV1::Output(page) => {
                assert_eq!(
                    page.chunks
                        .iter()
                        .map(|chunk| chunk.bytes.len())
                        .sum::<usize>(),
                    MAX_PAGE_BYTES as usize
                );
                assert!(
                    page.chunks
                        .iter()
                        .all(|chunk| chunk.bytes.iter().all(|byte| *byte == 255))
                );
            }
            _ => panic!("expected output page"),
        }
        server.join().unwrap();
    }
}
