// The reverse HTTP connection of a slideshow ("PTTH/1.0"): the client upgrades one of its
// connections with POST /reverse and from then on we send it requests (`GET
// /slideshows/1/assets/<n>`) and read its answers; its own events are acknowledged.
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::BytesMut;
use rustplay_core::Result;
use rustplay_core::rtsp::{ClientCodec, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use super::Hub;
use crate::video::{event_reply, take_event};

// Drives an upgraded connection until the client or the receiver closes it. `inbound` holds
// whatever the client already sent after its POST /reverse.
pub async fn serve(
    stream: &mut TcpStream,
    mut inbound: BytesMut,
    hub: Arc<Hub>,
    session_id: String,
    peer: SocketAddr,
    cancel: &CancellationToken,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
    let id = hub.add_link(&session_id, tx);
    tracing::info!(%peer, "canal inverso de fotos abierto (sesión {session_id})");
    let mut codec = ClientCodec;
    let mut outbound = BytesMut::with_capacity(4 * 1024);
    let result = async {
        loop {
            loop {
                if let Some(event) = take_event(&mut inbound) {
                    tracing::debug!(%peer, "evento del dispositivo: {} {}", event.method, event.uri);
                    stream.write_all(&event_reply(event.cseq.as_deref())).await?;
                    continue;
                }
                match codec.decode(&mut inbound) {
                    Ok(Some(response)) => hub.asset_response(&response),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(%peer, "dato inesperado en el canal inverso de fotos ({e}): se descarta");
                        inbound.clear();
                        break;
                    }
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return Ok(()),
                request = rx.recv() => {
                    let Some(request) = request else { return Ok(()) };
                    outbound.clear();
                    codec.encode(request, &mut outbound)?;
                    stream.write_all(&outbound).await?;
                }
                read = stream.read_buf(&mut inbound) => {
                    if read? == 0 {
                        tracing::info!(%peer, "el dispositivo ha cerrado el canal inverso de fotos");
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    hub.remove_link(id);
    if let Err(e) = &result {
        tracing::warn!(%peer, "el canal inverso de fotos se cierra por un error: {e}");
    }
    result
}
