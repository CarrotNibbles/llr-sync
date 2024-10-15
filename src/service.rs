use crate::protos::stratsync::*;
use crate::types::*;

use dotenvy_macro::dotenv;
use moka::sync::Cache;
use sqlx::{postgres::PgPoolOptions, types::Uuid};
use std::sync::Arc;
use std::time::Duration;
use strat_sync_server::{StratSync, StratSyncServer};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

#[tonic::async_trait]
impl StratSync for StratSyncService {
    type EventStream = ReceiverStream<Result<EventResponse, Status>>;

    async fn event(
        &self,
        request: Request<SubscriptionRequest>,
    ) -> Result<Response<Self::EventStream>, Status> {
        self.rpc_event(request).await
    }

    async fn clear_other_sessions(
        &self,
        request: Request<ClearOtherSessionsRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_clear_other_sessions(request).await
    }

    async fn elevate(&self, request: Request<ElevationRequest>) -> Result<Response<()>, Status> {
        self.rpc_elevate(request).await
    }

    async fn upsert_damage_option(
        &self,
        request: Request<UpsertDamageOptionRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_upsert_damage_option(request).await
    }

    async fn upsert_entry(
        &self,
        request: Request<UpsertEntryRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_upsert_entry(request).await
    }

    async fn delete_entry(
        &self,
        request: Request<DeleteEntryRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_delete_entry(request).await
    }

    async fn update_player_job(
        &self,
        request: Request<UpdatePlayerJobRequest>,
    ) -> Result<Response<()>, Status> {
        self.rpc_update_player_job(request).await
    }
}

pub async fn build_stratsync() -> StratSyncServer<StratSyncService> {
    let pool = PgPoolOptions::new()
        .max_connections(32)
        .connect(dotenv!("DATABASE_URL"))
        .await
        .expect("Unable to connect to database");

    let action_cache: Cache<String, Arc<Vec<ActionInfo>>> = Cache::builder().build();

    sqlx::query!(
        r#"SELECT id, job AS "job: String", cooldown, stacks
           FROM public.actions"#
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .for_each(|row| {
        let mut abilities = (*action_cache.get(&row.job).unwrap_or(Arc::new(vec![]))).to_owned();
        abilities.push(ActionInfo {
            id: row.id,
            cooldown: row.cooldown,
            stacks: row.stacks,
        });

        action_cache.insert(row.job.to_owned(), Arc::new(abilities))
    });

    let raid_cache: Cache<Uuid, Arc<RaidInfo>> = Cache::builder().build();

    let strategy_lock: Cache<Uuid, Arc<Mutex<()>>> = Cache::builder().build();
    let strategy_context: Cache<Uuid, Arc<StrategyContext>> = Cache::builder().build();

    let strategy_lock_cloned = strategy_lock.clone();
    let strategy_context_cloned = strategy_context.clone();
    let peer_context: Cache<String, Arc<PeerContext>> = Cache::builder()
        .time_to_idle(Duration::from_secs(12 * 60 * 60))
        .eviction_listener(move |k: Arc<String>, v: Arc<PeerContext>, _| {
            if !v.tx.is_closed() {
                let cloned_tx = v.tx.clone();
                tokio::spawn(async move {
                    cloned_tx
                        .send(Err(Status::aborted("Session expired")))
                        .await
                        .ok();
                });
            }

            let mut context = (*strategy_context_cloned.get(&v.strategy_id).unwrap()).clone();

            let peers_after: Vec<_> = context
                .peers
                .iter()
                .filter(|&peer_id| *peer_id != *k)
                .map(|peer_id| peer_id.to_owned())
                .collect();

            let elevated_peers_after: Vec<_> = context
                .elevated_peers
                .iter()
                .filter(|&peer_id| *peer_id != *k)
                .map(|peer_id| peer_id.to_owned())
                .collect();

            if peers_after.is_empty() {
                strategy_lock_cloned.invalidate(&v.strategy_id);
                strategy_context_cloned.invalidate(&v.strategy_id);
            } else {
                context.peers = peers_after;
                context.elevated_peers = elevated_peers_after;
                strategy_context_cloned.insert(v.strategy_id, Arc::new(context));
            }
        })
        .build();

    StratSyncServer::new(StratSyncService {
        pool,
        action_cache,
        raid_cache,
        strategy_lock,
        strategy_context,
        peer_context,
    })
}
