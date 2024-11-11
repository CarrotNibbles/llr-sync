use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use sqlx::types::Uuid;
use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_mutate_entries(
        &self,
        request: Request<MutateEntriesRequest>,
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

        let player_lookup: HashMap<Uuid, &Player> = strategy_context
            .players
            .iter()
            .map(|player| (Uuid::parse_str(&player.id).unwrap(), player))
            .collect();
        let mut action_lookup: HashMap<Uuid, ActionInfo> = HashMap::new();
        for job in player_lookup
            .values()
            .filter_map(|player| player.job.as_ref())
        {
            for action in self.action_cache.get(job).unwrap().iter() {
                action_lookup.insert(action.id, action.clone());
            }
        }

        let mut grouped_upserts: HashMap<(Uuid, Uuid), Vec<(Uuid, i32)>> = HashMap::new();
        for entry in &payload.upserts {
            let id = utils::parse_string_to_uuid(&entry.id, "id has an invalid format")?;
            let player_id =
                utils::parse_string_to_uuid(&entry.player, "player has an invalid format")?;
            let action_id =
                utils::parse_string_to_uuid(&entry.action, "action has an invalid format")?;
            let use_at = entry.use_at;

            if use_at < -MAX_COUNTDOWN || use_at > raid.duration {
                return Err(Status::invalid_argument("use_at is out of range"));
            }

            let player = player_lookup
                .get(&player_id)
                .ok_or_else(|| Status::failed_precondition("Player not found"))?;

            if player.job.is_none() {
                return Err(Status::failed_precondition(
                    "Cannot upsert entries with an empty job",
                ));
            }

            grouped_upserts
                .entry((player_id, action_id))
                .or_default()
                .push((id, use_at));
        }

        let mut entries_after = strategy_context.entries.clone();
        let mut accepted_deletes: Vec<Uuid> = Vec::new();
        let mut accepted_upserts: Vec<(Uuid, Uuid, Uuid, i32)> = Vec::new();
        let mut rejected_upserts: Vec<(Uuid, Uuid, Uuid, i32)> = Vec::new();

        for id in &payload.deletes {
            if let Some(entry) = strategy_context
                .entries
                .iter()
                .find(|entry| entry.id == *id)
            {
                if match grouped_upserts.get(&(
                    Uuid::parse_str(&entry.player).unwrap(),
                    Uuid::parse_str(&entry.action).unwrap(),
                )) {
                    Some(upserts) => upserts
                        .iter()
                        .any(|(upsert_id, _)| upsert_id.to_string() == *id),
                    None => false,
                } {
                    return Err(Status::invalid_argument(
                        "Cannot delete an entry that is being upserted",
                    ));
                }

                accepted_deletes.push(Uuid::parse_str(id).unwrap());
            }
        }

        entries_after
            .retain(|entry| !accepted_deletes.contains(&Uuid::parse_str(&entry.id).unwrap()));

        let keys_to_check: HashSet<_> = grouped_upserts
            .keys()
            .map(|(player_id, action_id)| (player_id.to_owned(), action_id.to_owned()))
            .collect();

        for (player_id, action_id) in keys_to_check {
            let action = action_lookup.get(&action_id).unwrap();

            let entries_col: Vec<_> = entries_after
                .iter()
                .filter(|entry| {
                    entry.player == player_id.to_string() && entry.action == action_id.to_string()
                })
                .cloned()
                .collect();

            let upserts_col = grouped_upserts.get(&(player_id, action_id));

            let mut use_at_prov_map: HashMap<Uuid, i32> = entries_col
                .into_iter()
                .map(|entry| (Uuid::parse_str(&entry.id).unwrap(), entry.use_at))
                .collect();

            if let Some(upserts_col) = upserts_col {
                use_at_prov_map.extend(upserts_col.iter().cloned());
            }

            let mut col_sweeping: Vec<(i32, i32)> = Vec::new();
            for use_at in use_at_prov_map.values() {
                col_sweeping.push((*use_at, 1));
                col_sweeping.push((*use_at + action.cooldown, -1));
            }
            col_sweeping.sort();

            let mut max_simultaneous_uses = 0;
            let mut current_uses = 0;
            for (_, delta) in col_sweeping {
                current_uses += delta;
                max_simultaneous_uses = max_simultaneous_uses.max(current_uses);
            }

            if max_simultaneous_uses <= action.charges {
                if let Some(upserts_col) = upserts_col {
                    accepted_upserts.extend(
                        upserts_col
                            .iter()
                            .map(|&(id, use_at)| (player_id, action_id, id, use_at)),
                    );
                }

                entries_after.retain(|entry| {
                    entry.player != player_id.to_string() || entry.action != action_id.to_string()
                });

                for (id, use_at) in use_at_prov_map {
                    entries_after.push(Entry {
                        id: id.to_string(),
                        player: player_id.to_string(),
                        action: action_id.to_string(),
                        use_at,
                    });
                }
            } else if let Some(upserts_col) = upserts_col {
                rejected_upserts.extend(
                    upserts_col
                        .iter()
                        .map(|&(id, use_at)| (player_id, action_id, id, use_at)),
                );
            }
        }

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.entries = entries_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        if !rejected_upserts.is_empty() {
            let current_entries_map: HashMap<String, i32> = strategy_context
                .entries
                .iter()
                .map(|entry| (entry.id.to_owned(), entry.use_at))
                .collect();

            let (entries_present, entries_not_present): (Vec<_>, Vec<_>) = rejected_upserts
                .into_iter()
                .partition(|(_, _, id, _)| current_entries_map.contains_key(&id.to_string()));

            let upserts_self: Vec<Entry> = entries_present
                .into_iter()
                .map(|(player_id, action_id, id, _)| Entry {
                    id: id.to_string(),
                    player: player_id.to_string(),
                    action: action_id.to_string(),
                    use_at: current_entries_map[&id.to_string()],
                })
                .collect();
            let deletes_self: Vec<String> = entries_not_present
                .into_iter()
                .map(|(_, _, id, _)| id.to_string())
                .collect();

            if !upserts_self.is_empty() || !deletes_self.is_empty() {
                let event = event_response::Event::MutateEntriesEvent(MutateEntriesEvent {
                    upserts: upserts_self,
                    deletes: deletes_self,
                });

                peer_context
                    .tx
                    .send(Ok(EventResponse { event: Some(event) }))
                    .await
                    .unwrap();
            }
        }

        let delete_query = if !accepted_deletes.is_empty() {
            let query = sqlx::query!(
                r#"DELETE FROM public.strategy_player_entries
                         WHERE id = ANY($1)"#,
                &accepted_deletes
            )
            .execute(&self.pool);

            Some(query)
        } else {
            None
        };

        let upsert_query = if !accepted_upserts.is_empty() {
            let (player_vec, action_vec, id_vec, use_at_vec) = accepted_upserts.iter().fold(
                (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
                |(mut player_vec, mut action_vec, mut id_vec, mut use_at_vec),
                 &(player, action, id, use_at)| {
                    player_vec.push(player);
                    action_vec.push(action);
                    id_vec.push(id);
                    use_at_vec.push(use_at);
                    (player_vec, action_vec, id_vec, use_at_vec)
                },
            );

            let query = sqlx::query!(
                r#"WITH data AS (SELECT *
                                   FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::int[])
                                     AS t(player, action, id, use_at))
               INSERT INTO public.strategy_player_entries (player, action, id, use_at)
                    SELECT * FROM data
               ON CONFLICT (id)
             DO UPDATE SET player = EXCLUDED.player,
                           action = EXCLUDED.action,
                           use_at = EXCLUDED.use_at"#,
                &player_vec,
                &action_vec,
                &id_vec,
                &use_at_vec
            )
            .execute(&self.pool);

            Some(query)
        } else {
            None
        };

        let update_modified_at_query = sqlx::query!(
            r#"SELECT update_modified_at ($1)"#,
            peer_context.strategy_id,
        )
        .execute(&self.pool);

        match (delete_query, upsert_query) {
            (Some(delete_query), Some(upsert_query)) => {
                tokio::try_join!(delete_query, upsert_query, update_modified_at_query).unwrap();
            }
            (Some(delete_query), None) => {
                tokio::try_join!(delete_query, update_modified_at_query).unwrap();
            }
            (None, Some(upsert_query)) => {
                tokio::try_join!(upsert_query, update_modified_at_query).unwrap();
            }
            (None, None) => {}
        }

        let upserts_broadcast: Vec<Entry> = accepted_upserts
            .into_iter()
            .map(|(player_id, action_id, id, use_at)| Entry {
                id: id.to_string(),
                player: player_id.to_string(),
                action: action_id.to_string(),
                use_at,
            })
            .collect();

        let deletes_broadcast: Vec<String> = accepted_deletes
            .into_iter()
            .map(|id| id.to_string())
            .collect();

        if !upserts_broadcast.is_empty() || !deletes_broadcast.is_empty() {
            let event = event_response::Event::MutateEntriesEvent(MutateEntriesEvent {
                upserts: upserts_broadcast,
                deletes: deletes_broadcast,
            });

            self.broadcast(&payload.token, &strategy_context, event)
                .await;
        }

        Ok(Response::new(()))
    }
}
