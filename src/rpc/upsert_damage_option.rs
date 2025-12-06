use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_upsert_damage_option(
        &self,
        request: Request<UpsertDamageOptionRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        tracing::info!("Upsert damage option request");

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        tracing::debug!(
            "Processing damage option upsert for strategy {}, version {}",
            peer_context.strategy_id,
            strategy_context.version
        );

        let raid = self
            .raid_cache
            .get(&strategy_context.raid_id)
            .ok_or_else(|| Status::internal("Raid data not found"))?;

        let damage_option = payload
            .damage_option
            .ok_or_else(|| Status::invalid_argument("No damage option specified"))?;
        let damage_id =
            utils::parse_string_to_uuid(&damage_option.damage, "Damage id has an invalid format")?;
        let primary_target_id = damage_option
            .primary_target
            .as_deref()
            .map(|id| utils::parse_string_to_uuid(id, "Primary target id has an invalid format"))
            .transpose()?;

        let num_shared = damage_option.num_shared;

        let damage = raid
            .damages
            .iter()
            .find(|damage| damage.id == damage_id)
            .ok_or_else(|| {
                tracing::warn!(
                    "Damage {} not found in raid {} for strategy {}",
                    damage_id,
                    strategy_context.raid_id,
                    peer_context.strategy_id
                );
                Status::failed_precondition("Damage not found or not belongs to the specified raid")
            })?;

        if let Some(s) = num_shared {
            if s > damage.max_shared {
                tracing::warn!(
                    "Invalid num_shared {} > max_shared {} for damage {} in strategy {}",
                    s,
                    damage.max_shared,
                    damage_id,
                    peer_context.strategy_id
                );
                return Err(Status::failed_precondition(
                    "num_shared is greater than max_shared",
                ));
            }
        }

        if let Some(s) = primary_target_id {
            if !strategy_context
                .players
                .iter()
                .any(|player| player.id == s.to_string())
            {
                tracing::warn!(
                    "Primary target {} not found in strategy {}",
                    s,
                    peer_context.strategy_id
                );
                return Err(Status::failed_precondition("Primary target not found"));
            }
        }

        tracing::info!(
            "Upserting damage option {} with num_shared {:?}, primary_target {:?} in strategy {}",
            damage_id,
            num_shared,
            primary_target_id,
            peer_context.strategy_id
        );

        let damage_options_after: Vec<_> = strategy_context
            .damage_options
            .iter()
            .filter(|damage_option| damage_option.damage != damage_id.to_string())
            .chain([damage_option.clone()].iter())
            .map(|damage_option| damage_option.to_owned())
            .collect();

        // Use database transaction for atomicity
        tracing::debug!(
            "Beginning transaction for upsert damage option in strategy {}",
            peer_context.strategy_id
        );
        let mut tx = self.pool.begin().await.map_err(|e| {
            tracing::error!("Failed to begin transaction: {:?}", e);
            Status::internal("Failed to begin transaction")
        })?;

        utils::with_db_timeout(
            sqlx::query!(
                r#"INSERT INTO public.strategy_damage_options
                        VALUES ($1, $2, $3, $4)
                   ON CONFLICT (strategy, damage)
                 DO UPDATE SET num_shared = EXCLUDED.num_shared,
                               primary_target = EXCLUDED.primary_target"#,
                peer_context.strategy_id,
                damage_id,
                num_shared,
                primary_target_id
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
        strategy_context_after.damage_options = damage_options_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        tracing::info!(
            "Updated strategy {} context: damage option {} configured, version {} -> {}",
            peer_context.strategy_id,
            damage_id,
            old_version,
            old_version + 1
        );

        tracing::debug!(
            "Broadcasting damage option update for strategy {}",
            peer_context.strategy_id
        );

        self.broadcast(
            &payload.token,
            &strategy_context,
            event_response::Event::UpsertDamageOptionEvent(UpsertDamageOptionEvent {
                damage_option: Some(damage_option),
            }),
        )
        .await;

        tracing::info!(
            "Upsert damage option completed successfully for strategy {}: damage {}",
            peer_context.strategy_id,
            damage_id
        );

        Ok(Response::new(()))
    }
}
