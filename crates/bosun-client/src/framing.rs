//! Newline framing (#4): one JSON message per line, ended by `\n`.

use std::io;

use bosun_api::rpc::MAX_LINE;
use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads one line without its `\n` (and without a `\r` before it). `None` at the end of the
/// stream; a partial last line without its newline is dropped.
///
/// # Errors
///
/// I/O errors; [`io::ErrorKind::InvalidData`] for a line longer than [`MAX_LINE`] bytes or
/// not UTF-8.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let limit = MAX_LINE as u64 + 1;
    let n = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut bytes)
        .await?;
    if n == 0 || bytes.last() != Some(&b'\n') {
        if bytes.len() as u64 >= limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a line longer than {MAX_LINE} bytes"),
            ));
        }
        return Ok(None);
    }
    bytes.pop();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes `message` as one line and flushes it.
///
/// # Errors
///
/// I/O errors, or a value that does not serialise.
pub async fn write_message<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use tokio::io::BufReader;

    use super::*;

    #[tokio::test]
    async fn lines_are_split_on_newlines() {
        let mut reader = BufReader::new(&b"{\"a\":1}\n{\"b\":2}\r\npartial"[..]);
        assert_eq!(read_line(&mut reader).await.unwrap().unwrap(), "{\"a\":1}");
        assert_eq!(read_line(&mut reader).await.unwrap().unwrap(), "{\"b\":2}");
        assert_eq!(read_line(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_overlong_line_is_refused() {
        let long = vec![b'x'; MAX_LINE + 10];
        let mut reader = BufReader::new(&long[..]);
        let err = read_line(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
