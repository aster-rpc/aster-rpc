//! Qualification receiver: original bytes and full IDs commit before the durable ACK.
use super::{types::*, wire::*};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
const EVENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("accepted_v1");

pub(crate) struct Receiver {
    pub endpoint: String,
    pub mode: Arc<AtomicU16>,
    pub attempts: Arc<Mutex<Vec<Vec<String>>>>,
    database: Arc<redb::Database>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Receiver {
    pub fn start(path: &std::path::Path, mode: u16) -> Self {
        let database = Arc::new(redb::Database::create(path).unwrap());
        let write = crate::durable_storage::durable_write(&database).unwrap();
        write.open_table(EVENTS).unwrap();
        write.commit().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/audit", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU16::new(mode));
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let (worker_db, worker_stop, worker_mode, worker_attempts) = (
            database.clone(),
            stop.clone(),
            mode.clone(),
            attempts.clone(),
        );
        let thread = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = serve(stream, &worker_db, &worker_mode, &worker_attempts);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("receiver accept: {error}"),
                }
            }
        });
        Self {
            endpoint,
            mode,
            attempts,
            database,
            stop,
            thread: Some(thread),
        }
    }
    pub fn destination(&self) -> Destination {
        Destination {
            endpoint: self.endpoint.clone(),
            receiver_identity: Some("persistent-fixture-v1".into()),
            authorization: None,
        }
    }
    pub fn count(&self) -> u64 {
        self.database
            .begin_read()
            .unwrap()
            .open_table(EVENTS)
            .unwrap()
            .len()
            .unwrap()
    }
    pub fn contains(&self, id: &str) -> bool {
        self.database
            .begin_read()
            .unwrap()
            .open_table(EVENTS)
            .unwrap()
            .get(id)
            .unwrap()
            .is_some()
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}
fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    write!(stream, "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())?;
    stream.write_all(body)
}
fn serve(
    mut stream: TcpStream,
    database: &redb::Database,
    mode: &AtomicU16,
    attempts: &Mutex<Vec<Vec<String>>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut length = None;
    let mut auth = false;
    let mut header_bytes = 0;
    loop {
        let mut line = String::new();
        let read = reader.by_ref().take(4097).read_line(&mut line)?;
        header_bytes += read;
        if read == 0 || read > 4096 || header_bytes > 8192 {
            return respond(&mut stream, 400, b"{}");
        }
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
            if key.eq_ignore_ascii_case("authorization") {
                auth = value.trim() == "Bearer rotated";
            }
        }
    }
    let Some(length) = length.filter(|v| *v <= MAX_WIRE_BYTES) else {
        return respond(&mut stream, 413, b"{}");
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: BatchRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return respond(&mut stream, 400, b"{}"),
    };
    if request.version != 1
        || request.events.is_empty()
        || request.events.len() > MAX_BATCH_EVENTS
        || request.batch_id != batch_id(request.events.iter().map(|e| e.event_id.as_str()))
    {
        return respond(&mut stream, 400, b"{}");
    }
    attempts
        .lock()
        .unwrap()
        .push(request.events.iter().map(|e| e.event_id.clone()).collect());
    let current = mode.load(Ordering::Acquire);
    if current == 3 && !auth {
        return respond(&mut stream, 401, b"{}");
    }
    if current == 4 {
        return respond(&mut stream, 200, b"{}");
    }
    if current == 5 {
        return respond(
            &mut stream,
            409,
            b"{\"version\":1,\"error\":\"integrity_conflict\"}",
        );
    }
    if current == 6 {
        return respond(&mut stream, 404, b"{}");
    }
    if current == 7 {
        return respond(&mut stream, 503, b"{}");
    }
    let mut decoded = Vec::new();
    let mut total = 0;
    for event in &request.events {
        let (envelope, bytes) = match verify_wire_event(event, MAX_EVENT_BYTES) {
            Ok(decoded) => decoded,
            Err(_) => {
                return respond(
                    &mut stream,
                    409,
                    b"{\"version\":1,\"error\":\"integrity_conflict\"}",
                )
            }
        };
        total += bytes.len();
        if total > MAX_BATCH_BYTES {
            return respond(&mut stream, 413, b"{}");
        }
        if current == 2 && envelope.payload["n"] == 2 {
            return respond(&mut stream, 422, b"{}");
        }
        decoded.push((event.event_id.as_str(), bytes));
    }
    let write = crate::durable_storage::durable_write(database).unwrap();
    {
        let mut table = write.open_table(EVENTS).unwrap();
        for (id, bytes) in &decoded {
            let old = table.get(*id).unwrap().map(|v| v.value().to_vec());
            if old.as_deref().is_some_and(|old| old != bytes) {
                return respond(
                    &mut stream,
                    409,
                    b"{\"version\":1,\"error\":\"integrity_conflict\"}",
                );
            }
            table.insert(*id, bytes.as_slice()).unwrap();
        }
    }
    write.commit().unwrap();
    // A lost reply occurs only after the original bytes are committed durably.
    if current == 1 {
        mode.store(0, Ordering::Release);
        return Ok(());
    }
    if current == 9 {
        return Ok(());
    }
    let body = serde_json::to_vec(&DurableAck {
        version: 1,
        batch_id: request.batch_id,
        durable: true,
    })
    .unwrap();
    respond(&mut stream, 200, &body)
}
