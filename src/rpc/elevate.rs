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

        tracing::info!("Elevation request for strategy");

        utils::open_strategy!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing elevation for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        if strategy_context.elevated_peers.contains(&payload.token) {
            tracing::warn!(
                "Elevation attempt for already elevated peer in strategy {}",
                peer_context.strategy_id
            );
            return Err(Status::failed_precondition("Already elevated"));
        }

        let row = utils::with_db_timeout(
            sqlx::query!(
                r#"SELECT password, is_editable
                     FROM public.strategies
                    WHERE id = $1"#,
                peer_context.strategy_id
            )
            .fetch_one(&self.pool),
        )
        .await?;
        let is_strategy_editable = row.is_editable;

        if !is_strategy_editable {
            tracing::warn!(
                "Elevation attempt for non-editable strategy {}",
                peer_context.strategy_id
            );
            return Err(Status::permission_denied("Strategy is not editable"));
        }

        let strategy_password = row.password.ok_or_else(|| {
            tracing::warn!(
                "Elevation attempt for strategy {} without password set",
                peer_context.strategy_id
            );
            Status::permission_denied("Strategy password is not set")
        })?;

        tracing::debug!(
            "Verifying password for elevation in strategy {}",
            peer_context.strategy_id
        );

        if !bcrypt::verify(payload.password.as_str(), strategy_password.as_str()).map_err(|e| {
            tracing::error!(
                "Password verification error for strategy {}: {:?}",
                peer_context.strategy_id,
                e
            );
            Status::internal("Password verification failed")
        })? {
            tracing::warn!(
                "Invalid password attempt for elevation in strategy {}",
                peer_context.strategy_id
            );
            return Err(Status::permission_denied("Invalid password"));
        }

        tracing::info!(
            "Password verified successfully for elevation in strategy {}",
            peer_context.strategy_id
        );

        let mut strategy_context_after = (*strategy_context).to_owned();
        let old_version = strategy_context_after.version;
        strategy_context_after.version += 1;
        strategy_context_after
            .elevated_peers
            .push(payload.token.clone());
        let elevated_peers_count = strategy_context_after.elevated_peers.len();
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        tracing::info!(
            "Elevation granted for strategy {}, {} elevated peers, version {} -> {}",
            peer_context.strategy_id,
            elevated_peers_count,
            old_version,
            old_version + 1
        );

        Ok(Response::new(()))
    }
}
