use crate::protos::stratsync::*;
use crate::types::*;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_clear_other_sessions(
        &self,
        request: Request<ClearOtherSessionsRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        let peer_context = self
            .peer_context
            .get(&payload.token)
            .ok_or(Status::unauthenticated("Token not found"))?;

        if !peer_context.is_author {
            return Err(Status::permission_denied(
                "Only the author can clear other sessions",
            ));
        }

        let lock = self.strategy_lock.get(&peer_context.strategy_id).unwrap();
        let _guard = lock.lock().await;

        let strategy_context = self
            .strategy_context
            .get(&peer_context.strategy_id)
            .ok_or(Status::unauthenticated("Strategy not opened"))?;

        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            self.peer_context.invalidate(peer);
        }

        Ok(Response::new(()))
    }
}
