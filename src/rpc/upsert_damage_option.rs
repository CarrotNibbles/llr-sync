use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tokio::task::JoinSet;
use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_upsert_damage_option(
        &self,
        request: Request<UpsertDamageOptionRequest>,
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
        let raid = self.raid_cache.get(&strategy_context.raid_id).unwrap();

        if strategy_context
            .elevated_peers
            .iter()
            .find(|&s| s == &payload.token)
            == None
        {
            return Err(Status::permission_denied("Not elevated"));
        }

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
            if strategy_context
                .players
                .iter()
                .find(|player| player.id == s.to_string())
                == None
            {
                return Err(Status::failed_precondition("Primary target not found"));
            }
        }

        let damage_options_after: Vec<_> = strategy_context
            .damage_options
            .iter()
            .filter(|damage_option| damage_option.damage != damage_id.to_string())
            .chain(vec![damage_option.clone()].iter())
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
                r#"UPDATE public.strategies
                          SET id = $1
                        WHERE id = $1"#,
                peer_context.strategy_id,
            )
            .execute(&self.pool),
        )
        .unwrap();

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.damage_options = damage_options_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        let mut messages = JoinSet::new();
        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            let tx = self.peer_context.get(peer).unwrap().tx.clone();
            let event = event_response::Event::UpsertDamageOptionEvent(UpsertDamageOptionEvent {
                damage_option: Some(damage_option.clone()),
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

        Ok(Response::new(()))
    }
}
