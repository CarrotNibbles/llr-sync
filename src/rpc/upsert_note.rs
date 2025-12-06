use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

const MAX_NOTE_LENGTH: usize = 128;

impl StratSyncService {
    pub async fn rpc_upsert_note(
        &self,
        request: Request<UpsertNoteRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        tracing::info!("Upsert note request");

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing note upsert for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        let raid = self
            .raid_cache
            .get(&strategy_context.raid_id)
            .ok_or_else(|| Status::internal("Raid data not found"))?;

        let note = payload
            .note
            .ok_or_else(|| Status::invalid_argument("No note specified"))?;

        let note_id = utils::parse_string_to_uuid(&note.id, "Note id has an invalid format")?;

        if note.block < 1 || note.block > raid.headcount + 1 {
            tracing::warn!(
                "Invalid note block {} for strategy {}, valid range: 1 to {}",
                note.block,
                peer_context.strategy_id,
                raid.headcount + 1
            );
            return Err(Status::invalid_argument("Block is out of range"));
        }

        if note.offset < 0f32 || note.offset > 1f32 {
            tracing::warn!(
                "Invalid note offset {} for strategy {}",
                note.offset,
                peer_context.strategy_id
            );
            return Err(Status::invalid_argument("Offset is out of range"));
        }

        if note.at < -MAX_COUNTDOWN || note.at > raid.duration {
            tracing::warn!(
                "Invalid note timestamp {} for strategy {}, valid range: {} to {}",
                note.at,
                peer_context.strategy_id,
                -MAX_COUNTDOWN,
                raid.duration
            );
            return Err(Status::invalid_argument("At is out of range"));
        }

        if note.content.len() > MAX_NOTE_LENGTH {
            tracing::warn!(
                "Note content too long ({} chars) for strategy {}, max: {}",
                note.content.len(),
                peer_context.strategy_id,
                MAX_NOTE_LENGTH
            );
            return Err(Status::invalid_argument("Note text is too long"));
        }

        tracing::info!(
            "Upserting note {} at position (block={}, offset={}, at={}) in strategy {}",
            note_id,
            note.block,
            note.offset,
            note.at,
            peer_context.strategy_id
        );

        // Use database transaction for atomicity
        tracing::debug!(
            "Beginning transaction for upsert note in strategy {}",
            peer_context.strategy_id
        );
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!("Failed to begin transaction: {:?}", e);
            Status::internal("Failed to begin transaction")
        })?;

        utils::with_db_timeout(
            sqlx::query!(
                r#"INSERT INTO public.notes
                        VALUES ($1, $2, $3, $4, $5, $6)
                   ON CONFLICT (id)
                 DO UPDATE SET block = EXCLUDED.block,
                               "offset" = EXCLUDED.offset,
                               at = EXCLUDED.at,
                               content = EXCLUDED.content"#,
                note_id,
                peer_context.strategy_id,
                note.block,
                note.offset,
                note.at,
                note.content,
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
            "Upsert note completed successfully for strategy {}: note {}",
            peer_context.strategy_id,
            note_id
        );

        Ok(Response::new(()))
    }
}
