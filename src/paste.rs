use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use bytes::Bytes;
use sqlx::PgPool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{broadcast, Mutex, Notify};
use yrs::{
    sync::{
        protocol::MSG_SYNC_UPDATE,
        Awareness, DefaultProtocol, Message, MessageReader, Protocol, SyncMessage,
    },
    updates::{
        decoder::{Decode, DecoderV1},
        encoder::Encode,
    },
    Doc, ReadTxn, StateVector, Transact, Update,
};

/// In-memory state for one paste document, shared by all connected clients.
pub struct PasteDoc {
    pub link_id: i64,
    pub awareness: Arc<Awareness>,
    pub tx: broadcast::Sender<Bytes>,
    pub dirty: Arc<Notify>,
}

#[derive(Default, Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<i64, Arc<PasteDoc>>>>,
}

impl Registry {
    pub async fn get_or_load(&self, pool: &PgPool, link_id: i64) -> Result<Arc<PasteDoc>> {
        let mut guard = self.inner.lock().await;
        if let Some(d) = guard.get(&link_id) {
            return Ok(d.clone());
        }
        let stored: Option<(Vec<u8>,)> =
            sqlx::query_as("SELECT doc_state FROM pastes WHERE link_id = $1")
                .bind(link_id)
                .fetch_optional(pool)
                .await?;
        let doc = Doc::new();
        if let Some((bytes,)) = stored {
            if !bytes.is_empty() {
                let update = Update::decode_v1(&bytes)?;
                doc.transact_mut().apply_update(update)?;
            }
        }
        let awareness = Arc::new(Awareness::new(doc));
        let (tx, _) = broadcast::channel::<Bytes>(128);
        let dirty = Arc::new(Notify::new());

        let paste = Arc::new(PasteDoc {
            link_id,
            awareness: awareness.clone(),
            tx: tx.clone(),
            dirty: dirty.clone(),
        });

        // Debounced persistence task.
        let pool2 = pool.clone();
        let aw2 = awareness.clone();
        let dirty2 = dirty.clone();
        tokio::spawn(async move {
            loop {
                dirty2.notified().await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                let encoded = aw2
                    .doc()
                    .transact()
                    .encode_state_as_update_v1(&StateVector::default());
                if let Err(e) = sqlx::query(
                    "UPDATE pastes SET doc_state = $1, updated_at = now() WHERE link_id = $2",
                )
                .bind(&encoded)
                .bind(link_id)
                .execute(&pool2)
                .await
                {
                    tracing::warn!(?e, link_id, "failed to persist paste");
                }
            }
        });

        guard.insert(link_id, paste.clone());
        Ok(paste)
    }
}

pub async fn handle_ws(mut ws: WebSocket, paste: Arc<PasteDoc>) {
    let mut rx = paste.tx.subscribe();

    // Initial handshake: send our SyncStep1 and an awareness snapshot.
    {
        let aw = &paste.awareness;
        let sv = aw.doc().transact().state_vector();
        let step1 = Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1();
        if ws.send(WsMessage::Binary(step1)).await.is_err() {
            return;
        }
        if let Ok(update) = aw.update() {
            let aw_msg = Message::Awareness(update).encode_v1();
            let _ = ws.send(WsMessage::Binary(aw_msg)).await;
        }
    }

    let protocol = DefaultProtocol;

    loop {
        tokio::select! {
            incoming = ws.recv() => {
                let Some(Ok(msg)) = incoming else { break; };
                match msg {
                    WsMessage::Binary(data) => {
                        if handle_incoming(&paste, &protocol, &mut ws, &data).await.is_err() {
                            break;
                        }
                    }
                    WsMessage::Close(_) => break,
                    WsMessage::Ping(p) => { let _ = ws.send(WsMessage::Pong(p)).await; }
                    _ => {}
                }
            }
            bcast = rx.recv() => {
                match bcast {
                    Ok(bytes) => {
                        if ws.send(WsMessage::Binary(bytes.to_vec())).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

async fn handle_incoming(
    paste: &PasteDoc,
    protocol: &DefaultProtocol,
    ws: &mut WebSocket,
    data: &[u8],
) -> Result<()> {
    let mut decoder = DecoderV1::new(yrs::encoding::read::Cursor::new(data));
    let mut reader = MessageReader::new(&mut decoder);
    while let Some(Ok(message)) = reader.next() {
        // Track whether this was a doc update so we can re-broadcast and mark dirty.
        let is_update = matches!(
            &message,
            Message::Sync(SyncMessage::Update(_)) | Message::Sync(SyncMessage::SyncStep2(_))
        );
        let update_bytes = if is_update {
            match &message {
                Message::Sync(SyncMessage::Update(u)) | Message::Sync(SyncMessage::SyncStep2(u)) => {
                    Some(u.clone())
                }
                _ => None,
            }
        } else {
            None
        };

        // Let the default protocol update the awareness/doc and compute a direct reply.
        let is_awareness = matches!(&message, Message::Awareness(_));
        let awareness_raw = if is_awareness {
            Some(message.encode_v1())
        } else {
            None
        };

        match protocol.handle_message(&paste.awareness, message) {
            Ok(Some(reply)) => {
                let _ = ws.send(WsMessage::Binary(reply.encode_v1())).await;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(?e, "protocol error");
                anyhow::bail!("protocol: {e}");
            }
        }

        if let Some(update) = update_bytes {
            let rebroadcast = Message::Sync(SyncMessage::Update(update)).encode_v1();
            let _ = paste.tx.send(Bytes::from(rebroadcast));
            paste.dirty.notify_one();
        }
        if let Some(raw) = awareness_raw {
            let _ = paste.tx.send(Bytes::from(raw));
        }
    }
    // suppress unused warning on MSG_SYNC_UPDATE in case we want it later
    let _ = MSG_SYNC_UPDATE;
    Ok(())
}
