use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_delete_entry(
        &self,
        request: Request<DeleteEntryRequest>,
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

        let id = utils::parse_string_to_uuid(&payload.id, "id has an invalid format")?;

        let mut entries_after = strategy_context.entries.clone();
        let mut delta_len = entries_after.len();
        entries_after = entries_after
            .iter()
            .filter(|entry| entry.id != id.to_string())
            .map(|entry| entry.to_owned())
            .collect();
        delta_len -= entries_after.len();

        if delta_len == 0 {
            return Err(Status::failed_precondition("Entry not found"));
        }

        tokio::try_join!(
            sqlx::query!(
                r#"DELETE FROM public.strategy_player_entries
                             WHERE id = $1"#,
                id,
            )
            .execute(&self.pool),
            sqlx::query!(
                r#"SELECT update_modified_at ($1)"#,
                peer_context.strategy_id,
            )
            .execute(&self.pool),
        )
        .unwrap();

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.entries = entries_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        self.broadcast(
            &payload.token,
            &strategy_context,
            event_response::Event::DeleteEntryEvent(DeleteEntryEvent { id: id.to_string() }),
        )
        .await;

        Ok(Response::new(()))
    }
}
