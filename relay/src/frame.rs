//! Length-prefixed JSON frames, the same framing as the LAN protocol.

use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

pub const MAX_FRAME: u32 = 8 * 1024 * 1024;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// `Ok(None)` on a clean end of stream before a new frame.
pub async fn read<R: AsyncRead + Unpin>(
    r: &mut R,
    wait: Duration,
) -> Result<Option<Value>, String> {
    timeout(wait, async {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
        let n = u32::from_be_bytes(len);
        if n > MAX_FRAME {
            return Err("frame too large".into());
        }
        let mut b = vec![0u8; n as usize];
        r.read_exact(&mut b).await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "timeout".to_string())?
}

pub async fn write<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<(), String> {
    let b = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    timeout(WRITE_TIMEOUT, async {
        w.write_all(&(b.len() as u32).to_be_bytes()).await?;
        w.write_all(&b).await?;
        w.flush().await
    })
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(|e| e.to_string())
}
