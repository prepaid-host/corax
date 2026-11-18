//! External command FIFO (`command_file`, OMD: `tmp/run/nagios.cmd`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Longest accepted external command line.
const MAX_LINE: usize = 1024 * 1024;

use crate::{CommandSource, Shared};

/// Create the FIFO (mode 0660) if it does not exist yet.
pub fn ensure_fifo(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.file_type().is_fifo() => return Ok(()),
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let c = CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: valid NUL-terminated path; mkfifo has no other preconditions.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o660) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // mkfifo honours the umask; enforce group write access for the web server.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
}

/// Read commands from the FIFO forever.
pub async fn serve(shared: Arc<Shared>, path: &Path) -> std::io::Result<()> {
    ensure_fifo(path)?;
    // Opening read-write keeps the FIFO open when all writers disconnect (no EOF spin).
    let rx = tokio::net::unix::pipe::OpenOptions::new().read_write(true).open_receiver(path)?;
    let mut reader = BufReader::with_capacity(256 * 1024, rx);
    let mut buf = Vec::with_capacity(1024);
    let mut oversized = false;
    loop {
        buf.clear();
        // Bounded read: an endless line must not exhaust memory.
        if (&mut reader).take(MAX_LINE as u64).read_until(b'\n', &mut buf).await? == 0 {
            return Ok(());
        }
        if !buf.ends_with(b"\n") {
            oversized = true; // the rest of this line is dropped
            continue;
        }
        if std::mem::take(&mut oversized) {
            tracing::warn!("dropped an external command longer than {MAX_LINE} bytes");
            continue;
        }
        // Non-UTF-8 input (e.g. Latin-1 comments) must not stop the reader.
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.trim().is_empty() {
            shared.submit_command(line, CommandSource::Pipe);
        }
    }
}
