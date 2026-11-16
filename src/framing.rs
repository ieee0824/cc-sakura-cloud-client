//! Bounded, cancellation-safe newline framing for local MCP/IPC.
use anyhow::{Result, ensure};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub struct Frames<R> {
    reader: BufReader<R>,
    pending: Vec<u8>,
}
impl<R: AsyncRead + Unpin> Frames<R> {
    pub fn new(input: R) -> Self {
        Self {
            reader: BufReader::new(input),
            pending: Vec::new(),
        }
    }
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            let buffer = self.reader.fill_buf().await?;
            if buffer.is_empty() {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(String::from_utf8(std::mem::take(&mut self.pending))?));
            }
            let end = buffer.iter().position(|b| *b == b'\n').map(|n| n + 1);
            let count = end.unwrap_or(buffer.len());
            ensure!(
                self.pending.len() + count <= MAX_FRAME,
                "local frame exceeds 16 MiB"
            );
            self.pending.extend_from_slice(&buffer[..count]);
            self.reader.consume(count);
            if end.is_some() {
                return Ok(Some(String::from_utf8(std::mem::take(&mut self.pending))?));
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_reads_keep_partial_frames() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let (input, mut writer) = tokio::io::duplex(128);
        let mut frames = Frames::new(input);
        writer.write_all(b"{\"id\":").await?;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), frames.next_line())
                .await
                .is_err()
        );
        writer.write_all(b"1}\n{\"id\":2}\n").await?;
        assert_eq!(frames.next_line().await?.unwrap(), "{\"id\":1}\n");
        assert_eq!(frames.next_line().await?.unwrap(), "{\"id\":2}\n");
        Ok(())
    }
    #[tokio::test]
    async fn oversized_input_is_rejected_before_json_parsing() -> Result<()> {
        let input = std::io::Cursor::new(vec![b'a'; MAX_FRAME + 1]);
        assert!(Frames::new(input).next_line().await.is_err());
        Ok(())
    }
}
