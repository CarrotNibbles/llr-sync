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

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        let raid = self.raid_cache.get(&strategy_context.raid_id).unwrap();

        let note = payload
            .note
            .ok_or_else(|| Status::invalid_argument("No note specified"))?;

        let note_id = utils::parse_string_to_uuid(&note.id, "Note id has an invalid format")?;

        if note.block < 1 || note.block > raid.headcount + 1 {
            return Err(Status::invalid_argument("Block is out of range"));
        }

        if note.offset < 0f32 || note.offset > 1f32 {
            return Err(Status::invalid_argument("Offset is out of range"));
        }

        if note.at < -MAX_COUNTDOWN || note.at > raid.duration {
            return Err(Status::invalid_argument("At is out of range"));
        }

        if note.content.len() > MAX_NOTE_LENGTH {
            return Err(Status::invalid_argument("Note text is too long"));
        }

        tokio::try_join!(
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
            .execute(&self.pool),
            sqlx::query!(
                r#"SELECT update_modified_at ($1)"#,
                peer_context.strategy_id,
            )
            .execute(&self.pool),
        )
        .unwrap();

        Ok(Response::new(()))
    }
}
