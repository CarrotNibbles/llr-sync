use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_elevate(
        &self,
        request: Request<ElevationRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        utils::open_strategy!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        if strategy_context.elevated_peers.contains(&payload.token) {
            return Err(Status::failed_precondition("Already elevated"));
        }

        let row = sqlx::query!(
            r#"SELECT password, is_editable
                 FROM public.strategies
                WHERE id = $1"#,
            peer_context.strategy_id
        )
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let is_strategy_editable = row.is_editable;

        if !is_strategy_editable {
            return Err(Status::permission_denied("Strategy is not editable"));
        }

        let strategy_password = row
            .password
            .ok_or_else(|| Status::permission_denied("Strategy password is not set"))?;

        if !bcrypt::verify(payload.password.as_str(), strategy_password.as_str()).unwrap_or(false) {
            return Err(Status::permission_denied("Invalid password"));
        }

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after
            .elevated_peers
            .push(payload.token.clone());
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        Ok(Response::new(()))
    }
}
