use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sqlx::types::Uuid;
use std::{
    env,
    sync::{Arc, OnceLock},
};
use tokio::task::JoinSet;
use tonic::{metadata::MetadataMap, Status};

use crate::{
    protos::stratsync::{event_response, EventResponse},
    types::*,
};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    pub aud: String,
    pub exp: usize,
    pub iat: usize,
    pub iss: String,
    pub sub: String,
}

pub fn parse_string_to_uuid(id: &str, message: impl Into<String>) -> Result<Uuid, Status> {
    Uuid::parse_str(id).map_err(|_| Status::invalid_argument(message))
}

static DECODING_KEY: OnceLock<DecodingKey> = OnceLock::new();

pub fn parse_authorization_header(metadata: &MetadataMap) -> Result<Option<Uuid>, Status> {
    let decoding_key = DECODING_KEY.get_or_init(|| {
        let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set in the environment");
        DecodingKey::from_secret(jwt_secret.as_bytes())
    });

    let authorization = match metadata.get("authorization") {
        Some(value) => value,
        None => return Ok(None),
    };

    let authorization_as_string = authorization
        .to_str()
        .map_err(|_| Status::invalid_argument("Invalid authorization header"))?;

    let (token_type, token) = authorization_as_string
        .split_once(" ")
        .ok_or_else(|| Status::invalid_argument("Authorization header is malformed"))?;

    if token_type != "Bearer" {
        return Err(Status::invalid_argument("Authorization token must be of type 'Bearer'"));
    }

    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_audience(&["authenticated"]);

    let claims = decode::<Claims>(token, decoding_key, &validation)
        .map_err(|err| match err.kind() {
            jsonwebtoken::errors::ErrorKind::InvalidToken => {
                Status::unauthenticated("Invalid token")
            }
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                Status::unauthenticated("Token has expired")
            }
            _ => Status::unauthenticated("Failed to decode token"),
        })?
        .claims;

    let user_id = parse_string_to_uuid(&claims.sub, "sub claim has an invalid format")?;

    Ok(Some(user_id))
}

macro_rules! open_strategy {
    ($self: ident, $token: expr, $peer_context:ident, $lock:ident, $guard:ident, $strategy_context:ident) => {
        let $peer_context = $self
            .peer_context
            .get($token)
            .ok_or_else(|| Status::unauthenticated("Invalid token or peer context not found"))?;
        let $lock = $self
            .strategy_lock
            .get(&$peer_context.strategy_id)
            .ok_or_else(|| Status::internal("Strategy lock not found"))?;
        let $guard = $lock.lock().await;
        let $strategy_context = $self
            .strategy_context
            .get(&$peer_context.strategy_id)
            .ok_or_else(|| Status::unauthenticated("Strategy context not opened"))?;
    };
}

macro_rules! open_strategy_elevated {
    ($self: ident, $token: expr, $peer_context:ident, $lock:ident, $guard:ident, $strategy_context:ident) => {
        utils::open_strategy!(
            $self,
            $token,
            $peer_context,
            $lock,
            $guard,
            $strategy_context
        );

        if !$strategy_context.elevated_peers.iter().any(|s| s == $token) {
            return Err(Status::permission_denied(
                "Insufficient permissions: peer is not elevated",
            ));
        }
    };
}

pub(crate) use open_strategy;
pub(crate) use open_strategy_elevated;

impl StratSyncService {
    pub async fn broadcast(
        &self,
        token: &String,
        strategy_context: &Arc<StrategyContext>,
        event: event_response::Event,
    ) {
        let mut tasks = JoinSet::new();

        for peer in &strategy_context.peers {
            if token == peer {
                continue;
            }

            let tx = match self.peer_context.get(peer) {
                Some(peer_context) => peer_context.tx.clone(),
                None => continue,
            };
            let event = event.clone();

            if tx.is_closed() {
                self.peer_context.invalidate(peer);
                continue;
            }

            tasks.spawn(async move {
                tx.send(Ok(EventResponse { event: Some(event) }))
                    .await
                    .unwrap()
            });
        }

        while (tasks.join_next().await).is_some() {}
    }
}
