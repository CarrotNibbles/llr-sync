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

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        let note_id = utils::parse_string_to_uuid(&payload.id, "Note id has an invalid format")?;

        tokio::try_join!(
            sqlx::query!(
                r#"DELETE FROM public.notes
                         WHERE id = $1 AND strategy = $2"#,
                note_id,
                peer_context.strategy_id,
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
