use bcrypt::verify;
use dotenvy::dotenv;
use moka::sync::Cache;
use sqlx::{postgres::PgPoolOptions, types::Uuid, Pool, Postgres};
use std::{env, str::FromStr, sync::Arc, time::Duration};
use stratsync::{
    event_response::Event,
    strat_sync_server::{StratSync, StratSyncServer},
    DamageOption, DeleteEntryEvent, DeleteEntryRequest, ElevationRequest, Entry, EventResponse,
    InitializationEvent, Player, SubscriptionRequest, UpdatePlayerJobEvent, UpdatePlayerJobRequest,
    UpsertDamageOptionEvent, UpsertDamageOptionRequest, UpsertEntryEvent, UpsertEntryRequest,
};
use strum_macros::EnumString;
use tokio::{
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    task::JoinSet,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};
use tonic_web::GrpcWebLayer;
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

pub mod stratsync {
    tonic::include_proto!("stratsync");
}

#[derive(Clone, Debug, PartialEq, PartialOrd, sqlx::Type, EnumString)]
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
    PCT,
    VPR,
}

#[derive(Debug, Clone)]
pub struct PeerContext {
    pub strategy_id: Uuid,
    pub raid_id: Uuid,
    pub tx: Sender<Result<EventResponse, Status>>,
}

#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub raid_id: Uuid,
    pub peers: Vec<String>,
    pub elevated_peers: Vec<String>,
    pub players: Vec<Player>,
    pub damage_options: Vec<DamageOption>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Damage {
    pub id: Uuid,
    pub max_shared: i32,
    pub num_targets: i32,
}

#[derive(Debug, Clone)]
pub struct RaidInfo {
    pub duration: i32,
    pub headcount: i32,
    pub damages: Vec<Damage>,
}

#[derive(Debug, Clone)]
pub struct ActionInfo {
    pub id: Uuid,
    pub cooldown: i32,
    pub stacks: i32,
}

pub struct StratSyncService {
    pub pool: Pool<Postgres>,
    pub action_cache: Cache<String, Arc<Vec<ActionInfo>>>,
    pub raid_cache: Cache<Uuid, Arc<RaidInfo>>,
    pub strategy_lock: Cache<Uuid, Arc<Mutex<()>>>,
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

        let strategy_id =
            parse_string_to_uuid(&payload.strategy, "Strategy id has an invalid format")?;

        let raid_id = sqlx::query!(
            r#"SELECT raid
                 FROM public.strategies
                WHERE id = $1"#,
            strategy_id
        )
        .fetch_one(&self.pool)
        .await
        .or(Err(Status::permission_denied("Access denied to strategy")))?
        .raid;

        if !self.raid_cache.contains_key(&raid_id) {
            let damages = sqlx::query_as!(
                Damage,
                r#"SELECT d.id, max_shared, num_targets
                     FROM public.damages AS d
                          JOIN public.gimmicks AS g
                          ON d.gimmick = g.id
                    WHERE g.raid = $1"#,
                raid_id
            )
            .fetch_all(&self.pool)
            .await
            .unwrap();

            let row = sqlx::query!(
                r#"SELECT duration, headcount
                     FROM public.raids
                    WHERE id = $1"#,
                raid_id
            )
            .fetch_one(&self.pool)
            .await
            .unwrap();

            self.raid_cache.insert(
                raid_id,
                Arc::new(RaidInfo {
                    duration: row.duration,
                    headcount: row.headcount,
                    damages,
                }),
            );
        }

        let lock = if let Some(lock) = self.strategy_lock.get(&strategy_id) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            self.strategy_lock.insert(strategy_id, lock.clone());
            lock
        };
        let _guard = lock.lock().await;

        let token = Uuid::new_v4().to_string();

        let peers: Vec<String> =
            if let Some(strategy_context) = self.strategy_context.get(&strategy_id) {
                strategy_context
                    .peers
                    .iter()
                    .chain([token.clone()].iter())
                    .map(|el| el.to_owned())
                    .collect()
            } else {
                vec![token.clone()]
            };

        let elevated_peers: Vec<String> =
            if let Some(strategy_context) = self.strategy_context.get(&strategy_id) {
                strategy_context.elevated_peers.clone()
            } else {
                vec![]
            };

        let players: Vec<Player>;
        let damage_options: Vec<DamageOption>;
        let entries: Vec<Entry>;
        if peers.len() > 1 {
            let mut strategy_context =
                (*self.strategy_context.get(&strategy_id).unwrap()).to_owned();
            strategy_context.peers = peers;

            players = strategy_context.players.clone();
            damage_options = strategy_context.damage_options.clone();
            entries = strategy_context.entries.clone();

            self.strategy_context
                .insert(strategy_id, Arc::new(strategy_context))
        } else {
            players = sqlx::query_as!(
                Player,
                r#"  WITH ordered_table AS (SELECT *
                                            FROM public.strategy_players
                                            ORDER BY "order")
                   SELECT id, job AS "job: String", "order"
                     FROM ordered_table
                    WHERE strategy = $1"#,
                strategy_id
            )
            .fetch_all(&self.pool)
            .await
            .unwrap();

            damage_options = sqlx::query!(
                r#"SELECT damage, num_shared, primary_target
                     FROM public.strategy_damage_options
                    WHERE strategy = $1"#,
                strategy_id
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
            .iter()
            .map(|record| DamageOption {
                damage: record.damage.to_string(),
                num_shared: record.num_shared,
                primary_target: record.primary_target.map(|s| s.to_string()),
            })
            .collect();

            entries = sqlx::query_as!(
                Entry,
                r#"SELECT e.id AS id, player, action, use_at
                     FROM public.strategy_player_entries AS e
                          JOIN public.strategy_players AS p
                          ON e.player = p.id
                    WHERE p.strategy = $1"#,
                strategy_id
            )
            .fetch_all(&self.pool)
            .await
            .unwrap();

            self.strategy_context.insert(
                strategy_id,
                Arc::new(StrategyContext {
                    raid_id,
                    peers,
                    elevated_peers,
                    players: players.clone(),
                    damage_options: damage_options.clone(),
                    entries: entries.clone(),
                }),
            );
        }

        let (tx, rx) = mpsc::channel(32);
        self.peer_context.insert(
            token.clone(),
            Arc::new(PeerContext {
                strategy_id,
                raid_id,
                tx: tx.clone(),
            }),
        );

        tx.send(Ok(EventResponse {
            event: Some(Event::InitializationEvent(InitializationEvent {
                token,
                players,
                damage_options,
                entries,
            })),
        }))
        .await
        .unwrap();

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn elevate(&self, request: Request<ElevationRequest>) -> Result<Response<()>, Status> {
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

        if strategy_context.elevated_peers.contains(&payload.token) {
            return Err(Status::failed_precondition("Already elevated"));
        }

        let strategy_password = sqlx::query!(
            r#"SELECT password
                 FROM public.strategies
                WHERE id = $1"#,
            peer_context.strategy_id
        )
        .fetch_one(&self.pool)
        .await
        .unwrap()
        .password;

        if !verify(payload.password.as_str(), strategy_password.as_str()).unwrap_or(false) {
            return Err(Status::permission_denied("Invalid password"));
        }

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after
            .elevated_peers
            .push(payload.token.clone());
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        Ok(Response::new(()))
    }

    async fn upsert_damage_option(
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
            parse_string_to_uuid(&damage_option.damage, "Damage id has an invalid format")?;
        let primary_target_id = match damage_option.primary_target {
            Some(ref id) => Some(parse_string_to_uuid(
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
            let event = Event::UpsertDamageOptionEvent(UpsertDamageOptionEvent {
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

    async fn upsert_entry(
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
        let id = parse_string_to_uuid(&entry.id, "id has an invalid format")?;
        let player_id = parse_string_to_uuid(&entry.player, "player has an invalid format")?;
        let action_id = parse_string_to_uuid(&entry.action, "action has an invalid format")?;
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
                Event::UpsertEntryEvent(UpsertEntryEvent {
                    entry: original_entry,
                })
            } else {
                Event::DeleteEntryEvent(DeleteEntryEvent { id: id.to_string() })
            };

            peer_context
                .tx
                .send(Ok(EventResponse { event: Some(event) }))
                .await
                .unwrap();
        } else {
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
            .execute(&self.pool)
            .await
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
                let event = Event::UpsertEntryEvent(UpsertEntryEvent {
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

    async fn delete_entry(
        &self,
        request: Request<DeleteEntryRequest>,
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

        let id = parse_string_to_uuid(&payload.id, "id has an invalid format")?;

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

        sqlx::query!(
            r#"DELETE FROM public.strategy_player_entries
                     WHERE id = $1"#,
            id,
        )
        .execute(&self.pool)
        .await
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
            let event = Event::DeleteEntryEvent(DeleteEntryEvent { id: id.to_string() });

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

    async fn update_player_job(
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

        let id = parse_string_to_uuid(&payload.id, "id has an invalid format")?;
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

        sqlx::query!(
            r#"UPDATE public.strategy_players
                  SET job = $1
                WHERE id = $2"#,
            job as Option<Job>,
            id,
        )
        .execute(&self.pool)
        .await
        .unwrap();

        sqlx::query!(
            r#"DELETE FROM public.strategy_player_entries
                     WHERE player = $1"#,
            id,
        )
        .execute(&self.pool)
        .await
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
            let event = Event::UpdatePlayerJobEvent(UpdatePlayerJobEvent {
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
            let mut abilities =
                (*action_cache.get(&row.job).unwrap_or(Arc::new(vec![]))).to_owned();
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
                let mut context = (*strategy_context_cloned.get(&v.strategy_id).unwrap()).clone();

                let peers_after: Vec<_> = context
                    .peers
                    .iter()
                    .filter(|&peer_id| *peer_id != *k)
                    .map(|peer_id| peer_id.to_owned())
                    .collect();

                if peers_after.len() == 0 {
                    strategy_lock_cloned.invalidate(&v.strategy_id);
                    strategy_context_cloned.invalidate(&v.strategy_id);
                } else {
                    context.peers = peers_after;
                    strategy_context_cloned.insert(v.strategy_id, Arc::new(context));
                }
            })
            .build();

        StratSyncService {
            pool,
            action_cache,
            raid_cache,
            strategy_lock,
            strategy_context,
            peer_context,
        }
    };

    Server::builder()
        .accept_http1(true)
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::mirror_request())
                .allow_headers(AllowHeaders::mirror_request()),
        )
        .layer(GrpcWebLayer::new())
        .add_service(StratSyncServer::new(stratsync_service))
        .serve(address)
        .await?;

    Ok(())
}
