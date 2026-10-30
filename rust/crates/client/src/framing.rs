//! Length-prefixed JSON framing, shared by the control connection and the local
//! CLI socket: `[len: u32 big-endian][json]`.

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Same ceiling the server enforces, so neither side can be made to allocate
/// without bound.
pub const MAX_FRAME_SIZE: u32 = 4 * 1024 * 1024;

pub async fn write_frame<W, T>(writer: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// Returns `Ok(None)` on a clean end of stream.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_FRAME_SIZE {
        bail!("frame of {len} bytes exceeds the limit");
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).context("malformed frame")?))
}
