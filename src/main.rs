use dotenvy::dotenv;
use moka::sync::Cache;
use sqlx::{postgres::PgPoolOptions, types::Uuid, Pool, Postgres};
use std::{env, sync::Arc, time::Duration};
use stratsync::{
    event_response::Event,
    strat_sync_server::{StratSync, StratSyncServer},
    AuthorizationEvent, DeleteBoxRequest, DeletePlayerRequest, EventResponse, SubscriptionRequest,
    UpsertBoxRequest, UpsertDamageOptionEvent, UpsertDamageOptionRequest, UpsertPlayerRequest,
};
use tokio::sync::{
    mpsc::{self, Sender},
    Mutex,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};

pub mod stratsync {
    tonic::include_proto!("stratsync");
}

#[derive(Clone, Debug, PartialEq, PartialOrd, sqlx::Type)]
#[sqlx(type_name = "job")]
pub enum Job {
    PLD,
    WAR,
    DRK,
    GNB,
    WHM,
    AST,
    SCH,
    SGE,
    MNK,
    DRG,
    NIN,
    SAM,
    RPR,
    BRD,
    MCH,
    DNC,
    BLM,
    RDM,
    SMN,
    BLU,
    LB,
}

#[derive(Debug, Clone)]
pub struct PeerContext {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub strategy_id: Uuid,
    pub raid_id: Uuid,
    pub tx: Sender<Result<EventResponse, Status>>,
}

#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub peers: Vec<String>,
    pub global_lock: Arc<Mutex<()>>,
    pub damage_option_locks: Cache<Uuid, Arc<Mutex<()>>>,
    pub box_locks: Cache<(Uuid, Uuid), Arc<Mutex<()>>>,
}

pub struct StratSyncService {
    pub pool: Pool<Postgres>,
    pub strategy_context: Cache<Uuid, Arc<StrategyContext>>,
    pub peer_context: Cache<String, Arc<PeerContext>>,
}

fn parse_string_to_uuid(id: &String, message: impl Into<String>) -> Result<Uuid, Status> {
    Uuid::parse_str(id.as_str()).or(Err(Status::invalid_argument(message)))
}

#[tonic::async_trait]
impl StratSync for StratSyncService {
    type EventStream = ReceiverStream<Result<EventResponse, Status>>;

    async fn event(
        &self,
        request: Request<SubscriptionRequest>,
    ) -> Result<Response<Self::EventStream>, Status> {
        let payload = request.into_inner();

        let session_id =
            parse_string_to_uuid(&payload.session, "Session id has an invalid format")?;
        let strategy_id =
            parse_string_to_uuid(&payload.strategy, "Strategy id has an invalid format")?;

        let user_id = sqlx::query!(
            r#"SELECT user_id
                 FROM auth.sessions
                WHERE id = $1"#,
            session_id
        )
        .fetch_one(&self.pool)
        .await
        .or(Err(Status::unauthenticated("Session not found")))?
        .user_id;

        let raid_id = sqlx::query!(
            r#"SELECT raid
                 FROM public.strategies
                WHERE id = $1
                      AND author = $2"#,
            strategy_id,
            user_id
        )
        .fetch_one(&self.pool)
        .await
        .or(Err(Status::permission_denied("Access denied to strategy")))?
        .raid;

        let token = Uuid::new_v4().to_string();

        let peers: Vec<String> = if let Some(peer_context) = self.strategy_context.get(&strategy_id)
        {
            peer_context
                .peers
                .iter()
                .chain([token.clone()].iter())
                .map(|el| el.to_owned())
                .collect()
        } else {
            vec![token.clone()]
        };

        let damage_option_locks: Cache<Uuid, Arc<Mutex<()>>> = Cache::builder().build();
        sqlx::query!(
            r#"SELECT d.id
                 FROM public.damages AS d
                      JOIN public.gimmicks AS g
                      ON d.gimmick = g.id
                WHERE g.raid = $1"#,
            raid_id
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .iter()
        .for_each(|row| damage_option_locks.insert(row.id, Arc::new(Mutex::new(()))));

        let box_locks: Cache<(Uuid, Uuid), Arc<Mutex<()>>> = Cache::builder().build();
        sqlx::query!(
            r#"SELECT p.id as player_id, a.id as ability_id
                 FROM public.strategy_players AS p
                      JOIN public.abilities AS a
                      ON p.job = a.job
                WHERE p.strategy = $1"#,
            strategy_id
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
        .iter()
        .for_each(|row| {
            box_locks.insert((row.player_id, row.ability_id), Arc::new(Mutex::new(())))
        });

        let (tx, rx) = mpsc::channel(32);

        self.peer_context.insert(
            token.clone(),
            Arc::new(PeerContext {
                user_id,
                session_id,
                strategy_id,
                raid_id,
                tx: tx.clone(),
            }),
        );
        self.strategy_context.insert(
            strategy_id,
            Arc::new(StrategyContext {
                peers,
                global_lock: Arc::new(Mutex::new(())),
                damage_option_locks,
                box_locks,
            }),
        );

        tokio::spawn(async move {
            tx.send(Ok(EventResponse {
                event: Some(Event::AuthorizationEvent(AuthorizationEvent { token })),
            }))
            .await
            .unwrap();
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn upsert_damage_option(
        &self,
        request: Request<UpsertDamageOptionRequest>,
    ) -> Result<Response<()>, Status> {
        let payload = request.into_inner();

        let damage_id = parse_string_to_uuid(&payload.damage, "Damage id has an invalid format")?;
        let primary_target_id = match payload.primary_target {
            Some(ref id) => Some(parse_string_to_uuid(
                id,
                "Primary target id has an invalid format",
            )?),
            None => None,
        };
        let num_shared = payload.num_shared.map(|n| n as i16);

        let peer_context = self
            .peer_context
            .get(&payload.token)
            .ok_or(Status::unauthenticated("Token not found"))?;
        let strategy_context = self
            .strategy_context
            .get(&peer_context.strategy_id)
            .ok_or(Status::unauthenticated("Strategy not opened"))?;

        let row = sqlx::query!(
            r#"SELECT d.id as damage_id, max_shared, raid as raid_id
                 FROM public.damages AS d
                      JOIN public.gimmicks AS g
                      ON d.gimmick = g.id
                WHERE d.id = $1"#,
            damage_id
        )
        .fetch_one(&self.pool)
        .await
        .or(Err(Status::unauthenticated("Damage not found")))?;

        if row.raid_id != peer_context.raid_id {
            return Err(Status::failed_precondition(
                "Damage not belongs to the specified raid",
            ));
        }

        if let Some(s) = num_shared {
            if s > row.max_shared {
                return Err(Status::failed_precondition(
                    "num_shared is greater than max_shared",
                ));
            }
        }

        if let Some(s) = primary_target_id {
            sqlx::query!(
                r#"SELECT id
                     FROM public.strategy_players
                    WHERE id = $1
                          AND strategy = $2"#,
                s,
                peer_context.strategy_id
            )
            .fetch_one(&self.pool)
            .await
            .or(Err(Status::failed_precondition(
                "Primary target is not found",
            )))?;
        }

        let lock = strategy_context
            .damage_option_locks
            .get(&damage_id)
            .unwrap();
        let _guard = lock.lock().await;

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
        .execute(&self.pool)
        .await
        .unwrap();

        for peer in &strategy_context.peers {
            if &payload.token != peer {
                continue;
            }

            self.peer_context
                .get(peer)
                .unwrap()
                .tx
                .send(Ok(EventResponse {
                    event: Some(Event::UpsertDamageOptionEvent(UpsertDamageOptionEvent {
                        damage: payload.damage.clone(),
                        num_shared: payload.num_shared,
                        primary_target: payload.primary_target.clone(),
                    })),
                }))
                .await
                .unwrap();
        }

        Ok(Response::new(()))
    }

    async fn upsert_box(&self, request: Request<UpsertBoxRequest>) -> Result<Response<()>, Status> {
        unimplemented!()
    }

    async fn delete_box(&self, request: Request<DeleteBoxRequest>) -> Result<Response<()>, Status> {
        unimplemented!()
    }

    async fn upsert_player(
        &self,
        request: Request<UpsertPlayerRequest>,
    ) -> Result<Response<()>, Status> {
        unimplemented!()
    }

    async fn delete_player(
        &self,
        request: Request<DeletePlayerRequest>,
    ) -> Result<Response<()>, Status> {
        unimplemented!()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().expect(".env file not found");

    let address = "[::1]:8080".parse().unwrap();

    let stratsync_service = {
        let pool = PgPoolOptions::new()
            .max_connections(15)
            .connect(
                env::var("DATABASE_URL")
                    .expect("DATABASE_URL not found")
                    .as_str(),
            )
            .await
            .expect("Unable to connect to database");

        let strategy_context: Cache<Uuid, Arc<StrategyContext>> = Cache::builder().build();

        let strategy_context_cloned = strategy_context.clone();
        let peer_context: Cache<String, Arc<PeerContext>> = Cache::builder()
            .time_to_idle(Duration::from_secs(12 * 60 * 60))
            .eviction_listener(move |k: Arc<String>, v: Arc<PeerContext>, _| {
                let mut context = (*strategy_context_cloned.get(&v.strategy_id).unwrap()).clone();

                let peers_after: Vec<_> = context
                    .peers
                    .iter()
                    .filter(|&peer_id| *peer_id != *k)
                    .map(|peer_id| peer_id.to_owned())
                    .collect();

                if peers_after.len() == 0 {
                    strategy_context_cloned.invalidate(&v.strategy_id);
                } else {
                    context.peers = peers_after;
                    strategy_context_cloned.insert(v.strategy_id, Arc::new(context));
                }
            })
            .build();

        StratSyncService {
            pool,
            strategy_context,
            peer_context,
        }
    };

    Server::builder()
        .add_service(StratSyncServer::new(stratsync_service))
        .serve(address)
        .await?;

    Ok(())
}
