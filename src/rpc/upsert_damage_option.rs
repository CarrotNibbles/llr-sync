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

        utils::open_strategy_elevated!(
            self,
            &payload.token,
            peer_context,
            lock,
            _guard,
            strategy_context
        );

        let raid = self.raid_cache.get(&strategy_context.raid_id).unwrap();

        let damage_option = payload
            .damage_option
            .ok_or(Status::invalid_argument("No damage option specified"))?;
        let damage_id =
            utils::parse_string_to_uuid(&damage_option.damage, "Damage id has an invalid format")?;
        let primary_target_id = match damage_option.primary_target {
            Some(ref id) => Some(utils::parse_string_to_uuid(
                id,
                "Primary target id has an invalid format",
            )?),
            None => None,
        };
        let num_shared = damage_option.num_shared;

        let damage = raid
            .damages
            .iter()
            .find(|damage| damage.id == damage_id)
            .ok_or(Status::failed_precondition(
                "Damage not found or not belongs to the specified raid",
            ))?;

        if let Some(s) = num_shared {
            if s > damage.max_shared {
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
                return Err(Status::failed_precondition("Primary target not found"));
            }
        }

        let damage_options_after: Vec<_> = strategy_context
            .damage_options
            .iter()
            .filter(|damage_option| damage_option.damage != damage_id.to_string())
            .chain([damage_option.clone()].iter())
            .map(|damage_option| damage_option.to_owned())
            .collect();

        tokio::try_join!(
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
            .execute(&self.pool),
            sqlx::query!(
                r#"SELECT update_modified_at ($1)"#,
                peer_context.strategy_id,
            )
            .execute(&self.pool),
        )
        .unwrap();

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.damage_options = damage_options_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        self.broadcast(
            &payload.token,
            &strategy_context,
            event_response::Event::UpsertDamageOptionEvent(UpsertDamageOptionEvent {
                damage_option: Some(damage_option),
            }),
        )
        .await;

        Ok(Response::new(()))
    }
}
