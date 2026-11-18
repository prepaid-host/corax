//! Unix socket server.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use corax_core::Shared;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::exec::{self, Response};
use crate::query::{status, Request};

/// Maximum size of a single request.
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Concurrent client connections; further clients wait in the accept queue.
pub(crate) const MAX_CONNECTIONS: usize = 512;
/// Idle time after which a keep-alive connection is closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Serve Livestatus on `socket` until the task is aborted.
pub async fn serve(shared: Arc<Shared>, socket: PathBuf) -> anyhow::Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o660))?;
    tracing::info!("livestatus: listening on {}", socket.display());
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Transient (EMFILE, ECONNABORTED, …): back off, never stop serving.
                tracing::warn!("livestatus: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let shared = shared.clone();
        let Ok(permit) = connections.clone().acquire_owned().await else { return Ok(()) };
        tokio::spawn(async move {
            let _permit = permit;
            crate::stats().connection_opened();
            if let Err(e) = handle_connection(shared, stream).await {
                tracing::debug!("livestatus: connection error: {e}");
            }
            crate::stats().connection_closed();
        });
    }
}

/// Read one request: lines up to an empty line. `Ok(None)` on clean EOF.
async fn read_request(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> std::io::Result<Option<String>> {
    let mut request = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        // Bound every read so a client cannot grow a single line without limit.
        let budget = (MAX_REQUEST_BYTES + 1).saturating_sub(request.len()) as u64;
        let n = (&mut *reader).take(budget).read_line(&mut line).await?;
        if request.len() + line.len() > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "request too large"));
        }
        if n == 0 {
            // EOF: a partial request without terminating empty line is still answered
            return Ok((!request.trim().is_empty()).then_some(request));
        }
        if line.trim_end_matches(['\n', '\r']).is_empty() {
            if request.is_empty() {
                continue; // stray empty lines between requests
            }
            return Ok(Some(request));
        }
        request.push_str(&line);
        if request.len() > MAX_REQUEST_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "request too large"));
        }
    }
}

/// Process one request on a blocking thread (queries may scan log files).
pub async fn answer(shared: Arc<Shared>, text: String) -> Response {
    // Parsing compiles regexes: keep it off the async worker threads.
    let (request, text) = match tokio::task::spawn_blocking(move || (Request::parse(&text), text)).await {
        Ok(r) => r,
        Err(e) => {
            return Response {
                status: 500,
                body: format!("internal error: {e}\n").into_bytes(),
                fixed16: false,
                keepalive: false,
                has_output: true,
            }
        }
    };
    match request {
        Request::Get(q) => {
            if q.error.is_none() && q.wait_condition.is_some() {
                if let Err(e) = exec::wait_for(&shared, &q).await {
                    return Response {
                        status: status::BAD_REQUEST,
                        body: format!("{e}\n").into_bytes(),
                        fixed16: q.fixed16,
                        keepalive: q.keepalive,
                        has_output: true,
                    };
                }
            }
            let fixed16 = q.fixed16;
            match tokio::task::spawn_blocking(move || exec::run_query(&shared, &q)).await {
                Ok(r) => r,
                Err(e) => Response {
                    status: 500,
                    body: format!("internal error: {e}\n").into_bytes(),
                    fixed16,
                    keepalive: false,
                    has_output: true,
                },
            }
        }
        _ => exec::execute(&shared, &text),
    }
}

async fn handle_connection(shared: Arc<Shared>, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    loop {
        let request = match tokio::time::timeout(IDLE_TIMEOUT, read_request(&mut reader)).await {
            Ok(r) => r?,
            Err(_) => return Ok(()),
        };
        let Some(text) = request else { return Ok(()) };
        let response = answer(shared.clone(), text).await;
        let bytes = response.to_bytes();
        if !bytes.is_empty() {
            write.write_all(&bytes).await?;
            write.flush().await?;
        }
        if !response.keepalive {
            let _ = write.shutdown().await;
            return Ok(());
        }
    }
}
