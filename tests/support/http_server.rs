use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum ResponseMode {
    Ranges,
    IgnoreRanges,
    UnknownLength,
    Status(u16),
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub path: String,
    pub body: Vec<u8>,
    pub mode: ResponseMode,
    pub chunk_size: usize,
    pub chunk_delay: Duration,
}

impl ServerConfig {
    pub fn new(path: &str, body: Vec<u8>, mode: ResponseMode) -> Self {
        Self {
            path: path.to_owned(),
            body,
            mode,
            chunk_size: 16 * 1024,
            chunk_delay: Duration::ZERO,
        }
    }

    pub fn throttled(mut self, chunk_size: usize, chunk_delay: Duration) -> Self {
        self.chunk_size = chunk_size;
        self.chunk_delay = chunk_delay;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedRequest {
    pub path: String,
    pub headers: BTreeMap<String, String>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

pub struct HttpServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HttpServer {
    pub fn spawn(config: ServerConfig) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test HTTP server");
        listener
            .set_nonblocking(true)
            .expect("set test listener nonblocking");
        let address = listener.local_addr().expect("read test server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
        let server_stopping = Arc::clone(&stopping);

        let thread = thread::spawn(move || {
            let config = Arc::new(config);
            let mut connections = Vec::new();
            while !server_stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let config = Arc::clone(&config);
                        let requests = Arc::clone(&server_requests);
                        connections.push(thread::spawn(move || {
                            let _ = serve_connection(stream, &config, &requests);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
            for connection in connections {
                let _ = connection.join();
            }
        });

        Self {
            address,
            requests,
            stopping,
            thread: Some(thread),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("request log lock").clone()
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_connection(
    mut stream: TcpStream,
    config: &ServerConfig,
    requests: &Mutex<Vec<RecordedRequest>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(&stream)?;
    requests
        .lock()
        .expect("request log lock")
        .push(request.clone());

    if request.path != config.path {
        return write_status(&mut stream, 404);
    }

    match config.mode {
        ResponseMode::Ranges => write_range_response(&mut stream, config, &request),
        ResponseMode::IgnoreRanges => write_known_response(&mut stream, config, 200, &config.body),
        ResponseMode::UnknownLength => write_unknown_response(&mut stream, config),
        ResponseMode::Status(status) => write_status(&mut stream, status),
    }
}

fn read_request(stream: &TcpStream) -> std::io::Result<RecordedRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut headers = BTreeMap::new();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    Ok(RecordedRequest { path, headers })
}

fn write_range_response(
    stream: &mut TcpStream,
    config: &ServerConfig,
    request: &RecordedRequest,
) -> std::io::Result<()> {
    let Some(range) = request.header("range") else {
        return write_known_response(stream, config, 200, &config.body);
    };
    let Some((start, end)) = parse_range(range, config.body.len()) else {
        write!(
            stream,
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nContent-Range: bytes */{}\r\nConnection: close\r\n\r\n",
            config.body.len()
        )?;
        return stream.flush();
    };
    let body = &config.body[start..=end];
    write!(
        stream,
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nETag: \"rustypac-fixture\"\r\nLast-Modified: Sat, 01 Aug 2026 00:00:00 GMT\r\nConnection: close\r\n\r\n",
        body.len(),
        config.body.len()
    )?;
    write_body(stream, body, config.chunk_size, config.chunk_delay)
}

fn parse_range(value: &str, total: usize) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    let last = total.checked_sub(1)?;
    let end = if end.is_empty() {
        last
    } else {
        end.parse::<usize>().ok()?.min(last)
    };
    (start <= end).then_some((start, end))
}

fn write_known_response(
    stream: &mut TcpStream,
    config: &ServerConfig,
    status: u16,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nETag: \"rustypac-fixture\"\r\nLast-Modified: Sat, 01 Aug 2026 00:00:00 GMT\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    write_body(stream, body, config.chunk_size, config.chunk_delay)
}

fn write_unknown_response(stream: &mut TcpStream, config: &ServerConfig) -> std::io::Result<()> {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
    for chunk in config.body.chunks(config.chunk_size.max(1)) {
        if !config.chunk_delay.is_zero() {
            thread::sleep(config.chunk_delay);
        }
        write!(stream, "{:x}\r\n", chunk.len())?;
        stream.write_all(chunk)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

fn write_status(stream: &mut TcpStream, status: u16) -> std::io::Result<()> {
    let reason = match status {
        404 => "Not Found",
        410 => "Gone",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()
}

fn write_body(
    stream: &mut TcpStream,
    body: &[u8],
    chunk_size: usize,
    chunk_delay: Duration,
) -> std::io::Result<()> {
    for chunk in body.chunks(chunk_size.max(1)) {
        if !chunk_delay.is_zero() {
            thread::sleep(chunk_delay);
        }
        stream.write_all(chunk)?;
        stream.flush()?;
    }
    Ok(())
}
