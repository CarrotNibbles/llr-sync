use std::str::FromStr;
use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_update_player_job(
        &self,
        request: Request<UpdatePlayerJobRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        tracing::info!("Update player job request for player {}", payload.id);

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        let job_as_string = payload.job.clone();

        tracing::debug!(
            "Processing job update for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        let id = utils::parse_string_to_uuid(&payload.id, "id has an invalid format")?;
        let job = payload
            .job
            .as_deref()
            .map(|j| Job::from_str(j).map_err(|_| Status::invalid_argument("Invalid job")))
            .transpose()?;

        strategy_context
            .players
            .iter()
            .find(|player| player.id == id.to_string())
            .ok_or_else(|| {
                tracing::warn!(
                    "Player {} not found in strategy {}",
                    id,
                    peer_context.strategy_id
                );
                Status::failed_precondition("Player not found")
            })?;

        tracing::info!(
            "Updating player {} job to {:?} in strategy {}",
            id,
            job,
            peer_context.strategy_id
        );

        // Use database transaction for atomicity
        tracing::debug!(
            "Beginning transaction for update player job in strategy {}",
            peer_context.strategy_id
        );
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!("Failed to begin transaction: {:?}", e);
            Status::internal("Failed to begin transaction")
        })?;

        tracing::debug!("Updating player {} job in database", id);
        utils::with_db_timeout(
            sqlx::query!(
                r#"UPDATE public.strategy_players
                      SET job = $1
                    WHERE id = $2"#,
                job as Option<Job>,
                id,
            )
            .execute(&mut *tx),
        )
        .await?;

        tracing::debug!("Deleting entries for player {} due to job change", id);
        utils::with_db_timeout(
            sqlx::query!(
                r#"DELETE FROM public.strategy_player_entries
                         WHERE player = $1"#,
                id,
            )
            .execute(&mut *tx),
        )
        .await?;

        utils::with_db_timeout(
            sqlx::query!(
                r#"SELECT update_modified_at ($1)"#,
                peer_context.strategy_id,
            )
            .execute(&mut *tx),
        )
        .await?;

        // Commit transaction
        tracing::debug!(
            "Committing transaction for strategy {}",
            peer_context.strategy_id
        );
        tx.commit().await.map_err(|e| {
            tracing::error!(
                "Failed to commit transaction for strategy {}: {:?}",
                peer_context.strategy_id,
                e
            );
            Status::internal("Failed to commit transaction")
        })?;

        tracing::info!(
            "Transaction committed successfully for strategy {}",
            peer_context.strategy_id
        );

        // Optimistic locking: increment version
        let mut strategy_context_after = (*strategy_context).to_owned();
        let old_version = strategy_context_after.version;
        strategy_context_after.version += 1;

        if let Some(player) = strategy_context_after
            .players
            .iter_mut()
            .find(|player| player.id == id.to_string())
        {
            player.job = payload.job.clone();
        } else {
            tracing::error!(
                "Player {} not found in strategy context for strategy {}",
                id,
                peer_context.strategy_id
            );
            return Err(Status::internal("Player not found in strategy context"));
        }

        let deleted_entries = strategy_context_after
            .entries
            .iter()
            .filter(|entry| entry.player == id.to_string())
            .count();

        strategy_context_after
            .entries
            .retain(|entry| entry.player != id.to_string());

        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        tracing::info!(
            "Updated strategy {} context: player {} job changed, {} entries deleted, version {} -> {}",
            peer_context.strategy_id,
            id,
            deleted_entries,
            old_version,
            old_version + 1
        );

        tracing::debug!(
            "Broadcasting player job update for strategy {}",
            peer_context.strategy_id
        );

        self.broadcast(
            &payload.token,
            &strategy_context,
            event_response::Event::UpdatePlayerJobEvent(UpdatePlayerJobEvent {
                id: id.to_string(),
                job: job_as_string.clone(),
            }),
        )
        .await;

        tracing::info!(
            "Update player job completed successfully for strategy {}: player {} -> {:?}",
            peer_context.strategy_id,
            id,
            job_as_string
        );

        Ok(Response::new(()))
    }
}
