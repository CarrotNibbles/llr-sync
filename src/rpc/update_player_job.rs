use std::str::FromStr;
use std::sync::Arc;

use crate::protos::stratsync::*;
use crate::types::*;
use crate::utils;

use tokio::task::JoinSet;
use tonic::{Request, Response, Status};

impl StratSyncService {
    pub async fn rpc_update_player_job(
        &self,
        request: Request<UpdatePlayerJobRequest>,
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

        let job_as_string = payload.job.clone();

        let id = utils::parse_string_to_uuid(&payload.id, "id has an invalid format")?;
        let job = if let Some(j) = &payload.job {
            Some(Job::from_str(j).or(Err(Status::invalid_argument("Invalid job")))?)
        } else {
            None
        };

        strategy_context
            .players
            .iter()
            .find(|player| player.id == id.to_string())
            .ok_or(Status::failed_precondition("Player not found"))?;

        tokio::try_join!(
            sqlx::query!(
                r#"UPDATE public.strategy_players
                          SET job = $1
                        WHERE id = $2"#,
                job as Option<Job>,
                id,
            )
            .execute(&self.pool),
            sqlx::query!(
                r#"DELETE FROM public.strategy_player_entries
                             WHERE player = $1"#,
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
        strategy_context_after
            .players
            .iter_mut()
            .find(|player| player.id == id.to_string())
            .unwrap()
            .job = payload.job.clone();
        strategy_context_after
            .entries
            .retain(|entry| entry.player != id.to_string());
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        let mut messages = JoinSet::new();
        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            let tx = self.peer_context.get(peer).unwrap().tx.clone();
            let event = event_response::Event::UpdatePlayerJobEvent(UpdatePlayerJobEvent {
                id: id.to_string(),
                job: job_as_string.clone(),
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
