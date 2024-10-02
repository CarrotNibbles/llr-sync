use crate::protos::stratsync::*;
use crate::types::*;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_clear_other_sessions(
        &self,
        request: Request<ClearOtherSessionsRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        let (peer_context, strategy_context, lock) = self.open_strategy(&payload.token, true)?;

        if !peer_context.is_author {
            return Err(Status::permission_denied(
                "Only the author can clear other sessions",
            ));
        }

        let _guard = lock.lock().await;

        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            self.peer_context.invalidate(peer);
        }

        Ok(Response::new(()))
    }
}
