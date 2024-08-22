use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tokio::task::JoinSet;
use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_upsert_entry(
        &self,
        request: Request<UpsertEntryRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        let peer_context = self
            .peer_context
            .get(&payload.token)
            .ok_or(Status::unauthenticated("Token not found"))?;

        let lock = self.strategy_lock.get(&peer_context.strategy_id).unwrap();
        let _guard = lock.lock().await;

        let strategy_context = self
            .strategy_context
            .get(&peer_context.strategy_id)
            .ok_or(Status::unauthenticated("Strategy not opened"))?;

        if strategy_context
            .elevated_peers
            .iter()
            .find(|&s| s == &payload.token)
            == None
        {
            return Err(Status::permission_denied("Not elevated"));
        }

        let entry = payload
            .entry
            .ok_or(Status::invalid_argument("No entry specified"))?;
        let id = utils::parse_string_to_uuid(&entry.id, "id has an invalid format")?;
        let player_id = utils::parse_string_to_uuid(&entry.player, "player has an invalid format")?;
        let action_id = utils::parse_string_to_uuid(&entry.action, "action has an invalid format")?;
        let use_at = entry.use_at;

        let player = strategy_context
            .players
            .iter()
            .find(|player| player.id == player_id.to_string())
            .ok_or(Status::failed_precondition("Player not found"))?;

        let job = player.job.clone().ok_or(Status::failed_precondition(
            "Cannot upsert entries with an empty job",
        ))?;

        let action = self
            .action_cache
            .get(&job)
            .unwrap()
            .iter()
            .find(|action| action.id == action_id)
            .map(|action| action.to_owned())
            .ok_or(Status::failed_precondition("Action not found"))?;

        let mut entries_after = strategy_context.entries.clone();
        let mut original_entry: Option<Entry> = None;
        if let Some(s) = entries_after
            .iter_mut()
            .find(|entry| entry.id == id.to_string())
        {
            original_entry = Some(s.clone());
            *s = entry.clone();
        } else {
            entries_after.push(entry.clone());
        }

        let mut column_covering: Vec<_> = entries_after
            .iter()
            .filter(|entry| {
                entry.player == player_id.to_string() && entry.action == action_id.to_string()
            })
            .map(|entry| {
                (
                    entry.use_at,
                    entry.use_at
                        + if action.stacks > 1 {
                            0
                        } else {
                            action.cooldown
                        },
                )
            })
            .collect();

        column_covering.sort();

        let collides =
            (0..column_covering.len() - 1).any(|i| column_covering[i].1 > column_covering[i + 1].0);

        if collides {
            let event = if let Some(_) = original_entry {
                event_response::Event::UpsertEntryEvent(UpsertEntryEvent {
                    entry: original_entry,
                })
            } else {
                event_response::Event::DeleteEntryEvent(DeleteEntryEvent { id: id.to_string() })
            };

            peer_context
                .tx
                .send(Ok(EventResponse { event: Some(event) }))
                .await
                .unwrap();
        } else {
            tokio::try_join!(
                sqlx::query!(
                    r#"INSERT INTO public.strategy_player_entries
                                VALUES ($1, $2, $3, $4)
                           ON CONFLICT (id)
                         DO UPDATE SET player = EXCLUDED.player,
                                       action = EXCLUDED.action,
                                       use_at = EXCLUDED.use_at"#,
                    player_id,
                    action_id,
                    use_at,
                    id,
                )
                .execute(&self.pool),
                sqlx::query!(
                    r#"UPDATE public.strategies
                              SET id = $1
                            WHERE id = $1"#,
                    peer_context.strategy_id,
                )
                .execute(&self.pool),
            )
            .unwrap();

            let mut strategy_context_after = (*strategy_context).to_owned();
            strategy_context_after.entries = entries_after;
            self.strategy_context
                .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

            let mut messages = JoinSet::new();
            for peer in &strategy_context.peers {
                if &payload.token == peer {
                    continue;
                }

                let tx = self.peer_context.get(peer).unwrap().tx.clone();
                let event = event_response::Event::UpsertEntryEvent(UpsertEntryEvent {
                    entry: Some(entry.clone()),
                });

                if tx.is_closed() {
                    self.peer_context.invalidate(peer);
                    continue;
                }

                messages.spawn(async move {
                    tx.send(Ok(EventResponse { event: Some(event) }))
                        .await
                        .unwrap()
                });
            }

            while let Some(_) = messages.join_next().await {}
        }

        Ok(Response::new(()))
    }
}
