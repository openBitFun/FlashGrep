use std::{sync::mpsc, sync::Arc};

use super::{
    protocol::{RequestEnvelope, ResponseEnvelope, ServerMessage},
    repo::RepoManager,
};

mod connections;
mod notifications;
mod router;

#[cfg(test)]
mod tests;

use connections::ConnectionRegistry;
pub use notifications::ServiceNotificationEvent;
use router::protocol_error;

#[derive(Clone)]
pub struct DaemonService {
    repos: Arc<RepoManager>,
    connections: Arc<ConnectionRegistry>,
}

impl DaemonService {
    pub fn new() -> Self {
        let connections = Arc::new(ConnectionRegistry::default());
        let notify_connections = Arc::clone(&connections);
        Self {
            repos: Arc::new(RepoManager::with_notifier(Arc::new(move |event| {
                notify_connections.broadcast(event);
            }))),
            connections,
        }
    }

    pub fn shutdown_all(&self) {
        self.repos.shutdown_all();
    }

    #[allow(dead_code)]
    pub fn handle(&self, envelope: RequestEnvelope) -> ResponseEnvelope {
        let id = envelope.id;
        match self.handle_envelope(envelope) {
            Ok((result, _, _)) => ResponseEnvelope::success(id, result),
            Err(error) => ResponseEnvelope::error(id, protocol_error(error)),
        }
    }

    pub fn register_connection(&self, sender: mpsc::Sender<ServerMessage>) -> u64 {
        self.connections.register(sender)
    }

    pub fn unregister_connection(&self, connection_id: u64) {
        self.connections.unregister(connection_id);
    }

    pub fn handle_for_connection(
        &self,
        connection_id: Option<u64>,
        envelope: RequestEnvelope,
    ) -> Option<ResponseEnvelope> {
        let id = envelope.id;
        match self.handle_envelope(envelope) {
            Ok((result, initialize_caps, initialized)) => {
                if let (Some(connection_id), Some(capabilities)) = (connection_id, initialize_caps)
                {
                    self.connections
                        .set_capabilities(connection_id, capabilities);
                }
                if let (Some(connection_id), true) = (connection_id, initialized) {
                    self.connections.mark_initialized(connection_id);
                }
                id.map(|id| ResponseEnvelope::success(Some(id), result))
            }
            Err(error) => id.map(|id| ResponseEnvelope::error(Some(id), protocol_error(error))),
        }
    }
}

impl Default for DaemonService {
    fn default() -> Self {
        Self::new()
    }
}
