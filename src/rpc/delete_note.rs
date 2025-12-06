use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_delete_note(
        &self,
        request: Request<DeleteNoteRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        tracing::info!("Delete note request for note {}", payload.id);

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing note deletion for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        let note_id = utils::parse_string_to_uuid(&payload.id, "Note id has an invalid format")?;

        tracing::info!(
            "Deleting note {} from strategy {}",
            note_id,
            peer_context.strategy_id
        );

        // Use database transaction for atomicity
        tracing::debug!(
            "Beginning transaction for delete note in strategy {}",
            peer_context.strategy_id
        );
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!("Failed to begin transaction: {:?}", e);
            Status::internal("Failed to begin transaction")
        })?;

        utils::with_db_timeout(
            sqlx::query!(
                r#"DELETE FROM public.notes
                         WHERE id = $1 AND strategy = $2"#,
                note_id,
                peer_context.strategy_id,
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
            "Delete note completed successfully for strategy {}: note {}",
            peer_context.strategy_id,
            note_id
        );

        Ok(Response::new(()))
    }
}
