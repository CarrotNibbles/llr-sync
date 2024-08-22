use dotenvy_macro::dotenv;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sqlx::types::Uuid;
use tonic::{metadata::MetadataMap, Status};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    pub aud: String,
    pub exp: usize,
    pub iat: usize,
    pub iss: String,
    pub sub: String,
}

pub fn parse_string_to_uuid(id: &String, message: impl Into<String>) -> Result<Uuid, Status> {
    Uuid::parse_str(id.as_str()).or(Err(Status::invalid_argument(message)))
}

pub fn get_user_id_from_authorization(metadata: &MetadataMap) -> Result<Option<Uuid>, Status> {
    if let Some(authorization) = metadata.get("authorization") {
        let authorization_as_string = authorization.to_str().or(Err(Status::invalid_argument(
            "Invalid authorization header",
        )))?;

        let (token_type, token) = authorization_as_string
            .split_once(" ")
            .ok_or(Status::invalid_argument("Invalid authorization header"))?;

        if token_type != "Bearer" {
            return Err(Status::invalid_argument("Invalid token type"));
        }

        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_audience(&["authenticated"]);

        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(dotenv!("JWT_SECRET").as_ref()),
            &validation,
        )
        .or(Err(Status::invalid_argument("Invalid token")))?
        .claims;

        let user_id = parse_string_to_uuid(&claims.sub, "sub has an invalid format")?;

        Ok(Some(user_id))
    } else {
        Ok(None)
    }
}
