use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_clear_other_sessions(
        &self,
        request: Request<ClearOtherSessionsRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        tracing::info!("Clear other sessions request");

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing clear other sessions for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        if !peer_context.is_author {
            tracing::warn!(
                "Non-author attempted to clear sessions for strategy {}",
                peer_context.strategy_id
            );
            return Err(Status::permission_denied(
                "Only the author can clear other sessions",
            ));
        }

        let session_count = strategy_context.peers.len() - 1; // Exclude current session

        tracing::info!(
            "Clearing {} other sessions for strategy {}",
            session_count,
            peer_context.strategy_id
        );

        let mut cleared_count = 0;
        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            tracing::debug!("Invalidating peer session: {}", peer);
            self.peer_context.invalidate(peer);
            cleared_count += 1;
        }

        tracing::info!(
            "Cleared {} sessions for strategy {}",
            cleared_count,
            peer_context.strategy_id
        );

        Ok(Response::new(()))
    }
}
