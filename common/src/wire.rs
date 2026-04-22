use anyhow::{Context, Result};
use bincode::{Decode, Encode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

pub async fn write_msg<W, M>(writer: &mut W, msg: &M) -> Result<()>
where
    W: AsyncWrite + Unpin,
    M: Encode,
{
    let config = bincode::config::standard();
    let body = bincode::encode_to_vec(msg, config).context("failed to encode message")?;
    let len = u32::try_from(body.len())
        .with_context(|| format!("message of {} bytes exceeds u32 range", body.len()))?;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("failed to write length prefix")?;
    writer
        .write_all(&body)
        .await
        .context("failed to write message body")?;
    writer.flush().await.context("failed to flush writer")?;
    Ok(())
}

pub async fn read_msg<R, M>(reader: &mut R) -> Result<M>
where
    R: AsyncRead + Unpin,
    M: Decode<()>,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("failed to read length prefix")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        anyhow::bail!(
            "message length {} exceeds {} byte limit",
            len,
            MAX_MESSAGE_BYTES
        );
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("failed to read message body")?;
    let config = bincode::config::standard();
    let (msg, _) = bincode::decode_from_slice(&body, config).context("failed to decode message")?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[derive(Debug, PartialEq, Eq, Encode, Decode)]
    struct TestMsg {
        value: u32,
        text: String,
    }

    #[tokio::test]
    async fn roundtrip_single_message() {
        let (mut a, mut b) = duplex(1024);
        let msg = TestMsg {
            value: 42,
            text: "hello".into(),
        };
        write_msg(&mut a, &msg).await.unwrap();
        let received: TestMsg = read_msg(&mut b).await.unwrap();
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn roundtrip_multiple_messages_preserves_order() {
        let (mut a, mut b) = duplex(1024);
        let msg1 = TestMsg {
            value: 1,
            text: "first".into(),
        };
        let msg2 = TestMsg {
            value: 2,
            text: "second".into(),
        };
        write_msg(&mut a, &msg1).await.unwrap();
        write_msg(&mut a, &msg2).await.unwrap();
        let r1: TestMsg = read_msg(&mut b).await.unwrap();
        let r2: TestMsg = read_msg(&mut b).await.unwrap();
        assert_eq!(r1, msg1);
        assert_eq!(r2, msg2);
    }

    #[tokio::test]
    async fn rejects_oversized_length_prefix() {
        let (mut a, mut b) = duplex(8);
        let too_large = (MAX_MESSAGE_BYTES as u32 + 1).to_le_bytes();
        a.write_all(&too_large).await.unwrap();
        let result: Result<TestMsg> = read_msg(&mut b).await;
        assert!(result.is_err());
    }
}
