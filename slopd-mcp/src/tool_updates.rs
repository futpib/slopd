use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rmcp::RoleServer;
use rmcp::service::Peer;
use tokio::sync::watch;
use tokio::task::JoinSet;

pub(crate) type LegacyPeer = Mutex<Option<Peer<RoleServer>>>;

#[derive(Clone)]
pub(crate) struct ToolUpdates {
    changed: watch::Sender<()>,
    peers: Arc<Mutex<Vec<Weak<LegacyPeer>>>>,
}

impl Default for ToolUpdates {
    fn default() -> Self {
        Self {
            changed: watch::channel(()).0,
            peers: Arc::default(),
        }
    }
}

impl ToolUpdates {
    pub(crate) fn register(&self, slot: &Arc<LegacyPeer>, peer: Peer<RoleServer>) {
        let mut current = slot.lock().unwrap_or_else(|lock| lock.into_inner());
        if current.is_some() {
            return;
        }
        *current = Some(peer);
        let mut peers = self.peers.lock().unwrap_or_else(|lock| lock.into_inner());
        peers.retain(|peer| peer.strong_count() > 0);
        peers.push(Arc::downgrade(slot));
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(crate) async fn notify(&self) {
        self.changed.send_replace(());
        let slots: Vec<_> = {
            let mut peers = self.peers.lock().unwrap_or_else(|lock| lock.into_inner());
            peers.retain(|peer| peer.strong_count() > 0);
            peers.iter().filter_map(Weak::upgrade).collect()
        };
        let mut sends = JoinSet::new();
        for slot in slots {
            let peer = slot.lock().unwrap_or_else(|lock| lock.into_inner()).clone();
            if let Some(peer) = peer
                && !peer.is_transport_closed()
            {
                sends.spawn(async move { notify_peer(&peer).await });
            }
        }
        while sends.join_next().await.is_some() {}
    }
}

pub(crate) async fn notify_peer(peer: &Peer<RoleServer>) {
    match tokio::time::timeout(Duration::from_secs(2), peer.notify_tool_list_changed()).await {
        Ok(Ok(())) => tracing::debug!("sent MCP tool list change notification"),
        Ok(Err(error)) => tracing::debug!(%error, "MCP tool list notification not delivered"),
        Err(_) => tracing::warn!("MCP tool list notification timed out"),
    }
}
