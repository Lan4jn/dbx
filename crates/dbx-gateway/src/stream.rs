use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::{GatewayError, GatewayErrorCode};

const BUFFER_SIZE: usize = 64 * 1024;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub left_to_right: u64,
    pub right_to_left: u64,
}

pub async fn relay_websockets<A, B>(
    mut left: WebSocketStream<A>,
    mut right: WebSocketStream<B>,
    idle_timeout: Duration,
    mut stop: watch::Receiver<bool>,
) -> Result<StreamStats, GatewayError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut stats = StreamStats::default();
    loop {
        let event = timeout(idle_timeout, async {
            tokio::select! {
                message = left.next() => (true, message),
                message = right.next() => (false, message),
                _ = wait_for_stop(&mut stop) => return None,
            }
            .into()
        })
        .await
        .map_err(|_| stream_error("gateway stream idle timeout"))?;
        let Some((from_left, message)) = event else { break };
        let Some(Ok(message)) = message else { break };
        match message {
            Message::Binary(data) => {
                if from_left {
                    stats.left_to_right += data.len() as u64;
                    if !operation_until_stop(
                        async { right.send(Message::Binary(data)).await.is_ok() },
                        idle_timeout,
                        &mut stop,
                    )
                    .await
                    {
                        return Err(stream_error("gateway stream closed"));
                    }
                } else {
                    stats.right_to_left += data.len() as u64;
                    if !operation_until_stop(
                        async { left.send(Message::Binary(data)).await.is_ok() },
                        idle_timeout,
                        &mut stop,
                    )
                    .await
                    {
                        return Err(stream_error("gateway stream closed"));
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(data) if from_left => {
                if !operation_until_stop(
                    async { left.send(Message::Pong(data)).await.is_ok() },
                    idle_timeout,
                    &mut stop,
                )
                .await
                {
                    break;
                }
            }
            Message::Ping(data) => {
                if !operation_until_stop(
                    async { right.send(Message::Pong(data)).await.is_ok() },
                    idle_timeout,
                    &mut stop,
                )
                .await
                {
                    break;
                }
            }
            Message::Pong(_) => {}
            _ => return Err(stream_error("non-binary gateway data frame")),
        }
    }
    let _ = timeout(CLOSE_TIMEOUT, left.close(None)).await;
    let _ = timeout(CLOSE_TIMEOUT, right.close(None)).await;
    Ok(stats)
}

pub async fn relay_websocket_to_io<S, I>(
    mut socket: WebSocketStream<S>,
    mut io: I,
    idle_timeout: Duration,
    mut stop: watch::Receiver<bool>,
) -> Result<StreamStats, GatewayError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin,
{
    let mut stats = StreamStats::default();
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    loop {
        let event = timeout(idle_timeout, async {
            tokio::select! {
                message = socket.next() => DataEvent::Socket(message),
                read = io.read(&mut buffer) => DataEvent::Io(read),
                _ = wait_for_stop(&mut stop) => DataEvent::Stop,
            }
        })
        .await
        .map_err(|_| stream_error("gateway stream idle timeout"))?;
        match event {
            DataEvent::Socket(Some(Ok(Message::Binary(data)))) => {
                if !operation_until_stop(async { io.write_all(&data).await.is_ok() }, idle_timeout, &mut stop).await {
                    return Err(stream_error("local target write failed"));
                }
                stats.left_to_right += data.len() as u64;
            }
            DataEvent::Socket(Some(Ok(Message::Ping(data)))) => {
                if !operation_until_stop(
                    async { socket.send(Message::Pong(data)).await.is_ok() },
                    idle_timeout,
                    &mut stop,
                )
                .await
                {
                    break;
                }
            }
            DataEvent::Socket(Some(Ok(Message::Pong(_)))) => {}
            DataEvent::Io(Ok(0)) | DataEvent::Socket(None | Some(Ok(Message::Close(_)))) | DataEvent::Stop => break,
            DataEvent::Io(Ok(count)) => {
                if !operation_until_stop(
                    async { socket.send(Message::Binary(buffer[..count].to_vec().into())).await.is_ok() },
                    idle_timeout,
                    &mut stop,
                )
                .await
                {
                    return Err(stream_error("gateway stream closed"));
                }
                stats.right_to_left += count as u64;
            }
            DataEvent::Io(Err(_)) => return Err(stream_error("local target read failed")),
            DataEvent::Socket(_) => return Err(stream_error("invalid gateway data frame")),
        }
    }
    let _ = io.shutdown().await;
    let _ = timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    Ok(stats)
}

enum DataEvent {
    Socket(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    Io(std::io::Result<usize>),
    Stop,
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    while !*stop.borrow() && stop.changed().await.is_ok() {}
}

async fn operation_until_stop<F>(operation: F, idle_timeout: Duration, stop: &mut watch::Receiver<bool>) -> bool
where
    F: Future<Output = bool>,
{
    tokio::select! {
        result = timeout(idle_timeout, operation) => matches!(result, Ok(true)),
        _ = wait_for_stop(stop) => false,
    }
}

fn stream_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::TargetUnavailable, message: message.to_string() }
}
