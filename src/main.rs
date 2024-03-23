use dotenvy::dotenv;
use moka::sync::Cache;
use sqlx::{postgres::PgPoolOptions, types::Uuid, Pool, Postgres};
use std::{env, str::FromStr, sync::Arc, time::Duration};
use stratsync::{
    event_response::Event,
    strat_sync_server::{StratSync, StratSyncServer},
    DamageOption, DeleteEntryEvent, DeleteEntryRequest, DeletePlayerEvent, DeletePlayerRequest,
    Entry, EventResponse, InitializationEvent, InsertPlayerEvent, InsertPlayerRequest, Player,
    SubscriptionRequest, UpsertDamageOptionEvent, UpsertDamageOptionRequest, UpsertEntryEvent,
    UpsertEntryRequest,
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
    pub raid_id: Uuid,
    pub peers: Vec<String>,
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
pub struct AbilityInfo {
    pub id: Uuid,
    pub cooldown: i32,
    pub stacks: i32,
}

pub struct StratSyncService {
    pub pool: Pool<Postgres>,
    pub ability_cache: Cache<String, Arc<Vec<AbilityInfo>>>,
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
                r#"SELECT id, job AS "job: String"
                     FROM public.strategy_players
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
                r#"SELECT e.id AS id, player, ability, use_at
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
                user_id,
                session_id,
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

        let entry = payload
            .entry
            .ok_or(Status::invalid_argument("No entry specified"))?;
        let id = parse_string_to_uuid(&entry.id, "id has an invalid format")?;
        let player_id = parse_string_to_uuid(&entry.player, "player has an invalid format")?;
        let ability_id = parse_string_to_uuid(&entry.ability, "ability has an invalid format")?;
        let use_at = entry.use_at;

        let player = strategy_context
            .players
            .iter()
            .find(|player| player.id == player_id.to_string())
            .ok_or(Status::failed_precondition("Player not found"))?;

        let ability = self
            .ability_cache
            .get(&player.job)
            .unwrap()
            .iter()
            .find(|ability| ability.id == ability_id)
            .map(|ability| ability.to_owned())
            .ok_or(Status::failed_precondition("Ability not found"))?;

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
                entry.player == player_id.to_string() && entry.ability == ability_id.to_string()
            })
            .map(|entry| (entry.use_at, entry.use_at + ability.cooldown))
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
                               ability = EXCLUDED.ability,
                               use_at = EXCLUDED.use_at"#,
                player_id,
                ability_id,
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

            messages.spawn(async move {
                tx.send(Ok(EventResponse { event: Some(event) }))
                    .await
                    .unwrap()
            });
        }

        while let Some(_) = messages.join_next().await {}

        Ok(Response::new(()))
    }

    async fn insert_player(
        &self,
        request: Request<InsertPlayerRequest>,
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

        let player = payload
            .player
            .ok_or(Status::invalid_argument("No player specified"))?;
        let id = parse_string_to_uuid(&player.id, "id has an invalid format")?;
        let job =
            Job::from_str(player.job.as_str()).or(Err(Status::invalid_argument("Invalid job")))?;

        if let Some(_) = strategy_context
            .players
            .iter()
            .find(|player| player.id == id.to_string())
        {
            return Err(Status::failed_precondition("Duplicate player id"));
        }

        sqlx::query!(
            r#"INSERT INTO public.strategy_players
                    VALUES ($1, $2, $3)"#,
            id,
            peer_context.strategy_id,
            job as Job,
        )
        .execute(&self.pool)
        .await
        .unwrap();

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.players.push(player.clone());
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        let mut messages = JoinSet::new();
        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            let tx = self.peer_context.get(peer).unwrap().tx.clone();
            let event = Event::InsertPlayerEvent(InsertPlayerEvent {
                player: Some(player.clone()),
            });

            messages.spawn(async move {
                tx.send(Ok(EventResponse { event: Some(event) }))
                    .await
                    .unwrap()
            });
        }

        while let Some(_) = messages.join_next().await {}

        Ok(Response::new(()))
    }

    async fn delete_player(
        &self,
        request: Request<DeletePlayerRequest>,
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

        let id = parse_string_to_uuid(&payload.id, "id has an invalid format")?;

        let mut players_after = strategy_context.players.clone();
        let mut delta_len = players_after.len();
        players_after = players_after
            .iter()
            .filter(|player| player.id != id.to_string())
            .map(|player| player.to_owned())
            .collect();
        delta_len -= players_after.len();

        let mut entries_after = strategy_context.entries.clone();
        entries_after = entries_after
            .iter()
            .filter(|entry| entry.player != id.to_string())
            .map(|entry| entry.to_owned())
            .collect();

        if delta_len == 0 {
            return Err(Status::failed_precondition("Player not found"));
        }

        sqlx::query!(
            r#"DELETE FROM public.strategy_players
                     WHERE id = $1"#,
            id,
        )
        .execute(&self.pool)
        .await
        .unwrap();

        let mut strategy_context_after = (*strategy_context).to_owned();
        strategy_context_after.players = players_after;
        strategy_context_after.entries = entries_after;
        self.strategy_context
            .insert(peer_context.strategy_id, Arc::new(strategy_context_after));

        let mut messages = JoinSet::new();
        for peer in &strategy_context.peers {
            if &payload.token == peer {
                continue;
            }

            let tx = self.peer_context.get(peer).unwrap().tx.clone();
            let event = Event::DeletePlayerEvent(DeletePlayerEvent { id: id.to_string() });

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

        let ability_cache: Cache<String, Arc<Vec<AbilityInfo>>> = Cache::builder().build();

        sqlx::query!(
            r#"SELECT id, job AS "job: String", cooldown, stacks
               FROM public.abilities"#
        )
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .for_each(|row| {
            let mut abilities =
                (*ability_cache.get(&row.job).unwrap_or(Arc::new(vec![]))).to_owned();
            abilities.push(AbilityInfo {
                id: row.id,
                cooldown: row.cooldown,
                stacks: row.stacks,
            });

            ability_cache.insert(row.job.to_owned(), Arc::new(abilities))
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
            ability_cache,
            raid_cache,
            strategy_lock,
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
