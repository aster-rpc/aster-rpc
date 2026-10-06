//! Loopback OTLP faults: real listener refusal, accepted-but-unanswered POST,
//! and HTTP 503. Never accepts a failed-mode body as a successful export.
#![allow(dead_code)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    Http503 = 1,
    Stall = 2,
    Refused = 3,
}

pub struct Collector {
    addr: SocketAddr,
    mode: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    accepted: Arc<Mutex<Vec<Vec<u8>>>>,
    failed: Arc<Mutex<usize>>,
}

impl Collector {
    pub fn new(fault: Fault) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut this = Self {
            addr: listener.local_addr().unwrap(),
            mode: Arc::new(AtomicU8::new(fault as u8)),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            accepted: Arc::default(),
            failed: Arc::default(),
        };
        if matches!(fault, Fault::Refused) {
            drop(listener);
        } else {
            this.start(listener);
        }
        this
    }
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn assert_refused(&self) {
        assert_eq!(
            TcpStream::connect_timeout(&self.addr, Duration::from_secs(1))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::ConnectionRefused
        );
    }
    fn start(&mut self, listener: TcpListener) {
        listener.set_nonblocking(true).unwrap();
        self.stop.store(false, Ordering::SeqCst);
        let (stop, mode, accepted, failed) = (
            self.stop.clone(),
            self.mode.clone(),
            self.accepted.clone(),
            self.failed.clone(),
        );
        self.worker = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Ok((stream, _)) = listener.accept() {
                    let (mode, accepted, failed) = (mode.clone(), accepted.clone(), failed.clone());
                    std::thread::spawn(move || serve(stream, mode, accepted, failed));
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }));
    }
    fn close_listener(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
    pub fn recover(&mut self) {
        self.mode.store(0, Ordering::SeqCst);
        if self.worker.is_none() {
            self.start(TcpListener::bind(self.addr).unwrap());
        }
    }
    pub fn fail(&mut self, fault: Fault) {
        self.mode.store(fault as u8, Ordering::SeqCst);
        if matches!(fault, Fault::Refused) {
            self.close_listener();
        }
    }
    pub fn failed_requests(&self) -> usize {
        *self.failed.lock().unwrap()
    }
    pub fn accepted(&self) -> Vec<Vec<u8>> {
        self.accepted.lock().unwrap().clone()
    }
    pub fn wait_for(&self, marker: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(body) = self
                .accepted()
                .into_iter()
                .find(|body| body.windows(marker.len()).any(|w| w == marker))
            {
                return Some(body);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.mode.store(3, Ordering::SeqCst);
        self.close_listener();
    }
}

fn serve(
    mut stream: TcpStream,
    mode: Arc<AtomicU8>,
    accepted: Arc<Mutex<Vec<Vec<u8>>>>,
    failed: Arc<Mutex<usize>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut len = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                len = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let fault = mode.load(Ordering::SeqCst);
    if fault != 0 {
        *failed.lock().unwrap() += 1;
        while mode.load(Ordering::SeqCst) == 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode.load(Ordering::SeqCst) == 3 {
            return;
        }
        let _ = stream.write_all(
            b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        );
    } else {
        if first.contains("/v1/logs") {
            accepted.lock().unwrap().push(body);
        }
        let _ =
            stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
    }
}
