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

        tracing::info!(
            "Mutate entries request: {} upserts, {} deletes",
            payload.upserts.len(),
            payload.deletes.len()
        );

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing mutate entries for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        let raid = self
            .raid_cache
            .get(&strategy_context.raid_id)
            .ok_or_else(|| Status::internal("Raid data not found"))?;

        let player_lookup: HashMap<Uuid, &Player> = strategy_context
            .players
            .iter()
            .map(|player| {
                let uuid = Uuid::parse_str(&player.id).map_err(|e| {
                    tracing::error!(
                        "Corrupt player UUID in strategy {}: {} - {:?}",
                        peer_context.strategy_id,
                        player.id,
                        e
                    );
                    Status::internal("Data corruption detected: invalid player UUID")
                })?;
                Ok((uuid, player))
            })
            .collect::<Result<_, Status>>()?;
        let mut action_lookup: HashMap<Uuid, ActionInfo> = HashMap::new();
        for job in player_lookup
            .values()
            .filter_map(|player| player.job.as_ref())
        {
            if let Some(actions) = self.action_cache.get(job) {
                for action in actions.iter() {
                    action_lookup.insert(action.id, action.clone());
                }
            }
        }

        let mut grouped_upserts: HashMap<(Uuid, Uuid), Vec<(Uuid, i32)>> = HashMap::new();
        for entry in &payload.upserts {
            let id = utils::parse_string_to_uuid(&entry.id, "Entry id has an invalid format")?;
            let player_id =
                utils::parse_string_to_uuid(&entry.player, "Player id has an invalid format")?;
            let action_id =
                utils::parse_string_to_uuid(&entry.action, "Action id has an invalid format")?;
            let use_at = entry.use_at;

            if use_at < -MAX_COUNTDOWN || use_at > raid.duration {
                tracing::warn!(
                    "Invalid use_at {} for entry {} in strategy {}, valid range: {} to {}",
                    use_at,
                    id,
                    peer_context.strategy_id,
                    -MAX_COUNTDOWN,
                    raid.duration
                );
                return Err(Status::invalid_argument("use_at is out of range"));
            }

            let player = player_lookup
                .get(&player_id)
                .ok_or_else(|| Status::failed_precondition("Player not found"))?;

            if player.job.is_none() {
                tracing::warn!(
                    "Cannot upsert entry for player {} without job in strategy {}",
                    player_id,
                    peer_context.strategy_id
                );
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
                let player_uuid = Uuid::parse_str(&entry.player).map_err(|e| {
                    tracing::error!(
                        "Corrupt entry player UUID in strategy {}: {} - {:?}",
                        peer_context.strategy_id,
                        entry.player,
                        e
                    );
                    Status::internal("Data corruption detected: invalid entry player UUID")
                })?;
                let action_uuid = Uuid::parse_str(&entry.action).map_err(|e| {
                    tracing::error!(
                        "Corrupt entry action UUID in strategy {}: {} - {:?}",
                        peer_context.strategy_id,
                        entry.action,
                        e
                    );
                    Status::internal("Data corruption detected: invalid entry action UUID")
                })?;

                if match grouped_upserts.get(&(player_uuid, action_uuid)) {
                    Some(upserts) => upserts
                        .iter()
                        .any(|(upsert_id, _)| upsert_id.to_string() == *id),
                    None => false,
                } {
                    tracing::warn!(
                        "Cannot delete entry {} that is being upserted in strategy {}",
                        id,
                        peer_context.strategy_id
                    );
                    return Err(Status::invalid_argument(
                        "Cannot delete an entry that is being upserted",
                    ));
                }

                let uuid = Uuid::parse_str(id).map_err(|e| {
                    tracing::error!(
                        "Invalid delete UUID in strategy {}: {} - {:?}",
                        peer_context.strategy_id,
                        id,
                        e
                    );
                    Status::invalid_argument("Invalid entry ID format")
                })?;
                accepted_deletes.push(uuid);
            }
        }

        entries_after.retain(|entry| match Uuid::parse_str(&entry.id) {
            Ok(uuid) => !accepted_deletes.contains(&uuid),
            Err(e) => {
                tracing::error!("Corrupt entry UUID during retain: {} - {:?}", entry.id, e);
                false
            }
        });

        let keys_to_check: HashSet<_> = grouped_upserts
            .keys()
            .map(|(player_id, action_id)| (player_id.to_owned(), action_id.to_owned()))
            .collect();

        for (player_id, action_id) in keys_to_check {
            let Some(action) = action_lookup.get(&action_id) else {
                continue;
            };

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
                .map(|entry| {
                    let uuid = Uuid::parse_str(&entry.id).map_err(|e| {
                        tracing::error!(
                            "Corrupt entry UUID in entries_col: {} - {:?}",
                            entry.id,
                            e
                        );
                        Status::internal("Data corruption detected: invalid entry UUID")
                    })?;
                    Ok((uuid, entry.use_at))
                })
                .collect::<Result<_, Status>>()?;

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

        // Optimistic locking: increment version
        let mut strategy_context_after = (*strategy_context).to_owned();
        let old_version = strategy_context_after.version;
        strategy_context_after.version += 1;
        strategy_context_after.entries = entries_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        tracing::info!(
            "Accepted {} upserts, {} deletes for strategy {}, version {} -> {}",
            accepted_upserts.len(),
            accepted_deletes.len(),
            peer_context.strategy_id,
            old_version,
            old_version + 1
        );

        if !rejected_upserts.is_empty() {
            tracing::warn!(
                "Rejected {} upserts due to cooldown violations in strategy {}",
                rejected_upserts.len(),
                peer_context.strategy_id
            );
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

                let _ = peer_context
                    .tx
                    .send(Ok(EventResponse { event: Some(event) }))
                    .await;
            }
        }

        // Use database transaction for atomicity
        tracing::debug!(
            "Beginning transaction for mutate entries in strategy {}",
            peer_context.strategy_id
        );
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!("Failed to begin transaction: {:?}", e);
            Status::internal("Failed to begin transaction")
        })?;

        if !accepted_deletes.is_empty() {
            tracing::debug!(
                "Deleting {} entries from strategy {}",
                accepted_deletes.len(),
                peer_context.strategy_id
            );
            utils::with_db_timeout(
                sqlx::query!(
                    r#"DELETE FROM public.strategy_player_entries
                             WHERE id = ANY($1)"#,
                    &accepted_deletes
                )
                .execute(&mut *tx),
            )
            .await?;
        }

        if !accepted_upserts.is_empty() {
            tracing::debug!(
                "Upserting {} entries in strategy {}",
                accepted_upserts.len(),
                peer_context.strategy_id
            );
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

            utils::with_db_timeout(
                sqlx::query!(
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
                .execute(&mut *tx),
            )
            .await?;
        }

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
            let upserts_count = upserts_broadcast.len();
            let deletes_count = deletes_broadcast.len();

            tracing::debug!(
                "Broadcasting {} upserts and {} deletes for strategy {}",
                upserts_count,
                deletes_count,
                peer_context.strategy_id
            );
            let event = event_response::Event::MutateEntriesEvent(MutateEntriesEvent {
                upserts: upserts_broadcast,
                deletes: deletes_broadcast,
            });

            self.broadcast(&payload.token, &strategy_context, event)
                .await;

            tracing::info!(
                "Broadcast completed for strategy {}: {} upserts, {} deletes",
                peer_context.strategy_id,
                upserts_count,
                deletes_count
            );
        }

        tracing::info!(
            "Mutate entries completed successfully for strategy {}",
            peer_context.strategy_id
        );

        Ok(Response::new(()))
    }
}
