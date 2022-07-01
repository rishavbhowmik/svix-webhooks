// SPDX-FileCopyrightText: © 2022 Svix Authors
// SPDX-License-Identifier: MIT

use std::fmt::{Debug, Display};

use axum::{
    async_trait,
    extract::{Extension, FromRequest, Path, RequestParts, TypedHeader},
    headers::{authorization::Bearer, Authorization},
};
use chacha20poly1305::aead::{Aead, NewAead};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use ed25519_compact::*;

use jwt_simple::prelude::*;
use rand::Rng;
use sea_orm::DatabaseConnection;
use validator::Validate;

use crate::{
    cfg::Configuration,
    db::models::application,
    error::{Error, HttpError, Result},
};

use super::types::{ApplicationId, ApplicationIdOrUid, OrganizationId};

/// The default org_id we use (useful for generating JWTs when testing).
pub fn default_org_id() -> OrganizationId {
    OrganizationId("org_23rb8YdGqMT0qIzpgGwdXfHirMu".to_owned())
}

/// The default org_id we use (useful for generating JWTs when testing).
pub fn management_org_id() -> OrganizationId {
    OrganizationId("org_00000000000SvixManagement00".to_owned())
}

fn to_internal_server_error(x: impl Display) -> HttpError {
    tracing::error!("Error: {}", x);
    HttpError::internal_server_errer(None, None)
}

pub struct Permissions {
    pub type_: KeyType,
    pub org_id: OrganizationId,
    pub app_id: Option<ApplicationId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    Organization,
    Application,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CustomClaim {
    #[serde(rename = "org", default, skip_serializing_if = "Option::is_none")]
    organization: Option<String>,
}

#[async_trait]
impl<B> FromRequest<B> for Permissions
where
    B: Send,
{
    type Rejection = Error;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self> {
        let Extension(ref cfg) = Extension::<Configuration>::from_request(req)
            .await
            .map_err(to_internal_server_error)?;

        let TypedHeader(Authorization(bearer)) =
            TypedHeader::<Authorization<Bearer>>::from_request(req)
                .await
                .map_err(|_| HttpError::unauthorized(None, Some("Invalid token".to_string())))?;

        let claims = cfg
            .jwt_secret
            .key
            .verify_token::<CustomClaim>(bearer.token(), None)
            .map_err(|_| HttpError::unauthorized(None, Some("Invalid token".to_string())))?;

        let bad_token = |field: &str, id_type: &str| {
            HttpError::bad_request(
                Some("bad token".to_string()),
                Some(format!("`{}` is not a valid {} id", field, id_type)),
            )
        };

        // If there is an `org` field then it is an Application authentication
        if let Some(org_id) = claims.custom.organization {
            let org_id = OrganizationId(org_id);
            org_id
                .validate()
                .map_err(|_| bad_token("org", "organization"))?;

            if let Some(app_id) = claims.subject {
                let app_id = ApplicationId(app_id);
                app_id
                    .validate()
                    .map_err(|_| bad_token("sub", "application"))?;

                Ok(Permissions {
                    org_id,
                    app_id: Some(app_id),
                    type_: KeyType::Application,
                })
            } else {
                Err(HttpError::unauthorized(
                    None,
                    Some("Invalid token (missing `sub`).".to_string()),
                )
                .into())
            }
        }
        // Otherwsie it's an Organization authentication
        else if let Some(org_id) = claims.subject {
            let org_id = OrganizationId(org_id);
            org_id.validate().map_err(|_| {
                HttpError::bad_request(
                    Some("bad_token".to_string()),
                    Some("`sub' is not a valid organization id.".to_string()),
                )
            })?;
            Ok(Permissions {
                org_id,
                app_id: None,
                type_: KeyType::Organization,
            })
        } else {
            Err(
                HttpError::unauthorized(None, Some("Invalid token (missing `sub`).".to_string()))
                    .into(),
            )
        }
    }
}

pub struct AuthenticatedOrganization {
    pub permissions: Permissions,
}

#[async_trait]
impl<B> FromRequest<B> for AuthenticatedOrganization
where
    B: Send,
{
    type Rejection = Error;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self> {
        let permissions = Permissions::from_request(req).await?;
        match permissions.type_ {
            KeyType::Organization => {}
            KeyType::Application => {
                return Err(HttpError::permission_denied(None, None).into());
            }
        }

        Ok(AuthenticatedOrganization { permissions })
    }
}

#[derive(Deserialize)]
struct ApplicationPathParams {
    app_id: ApplicationIdOrUid,
}

pub struct AuthenticatedOrganizationWithApplication {
    pub permissions: Permissions,
    pub app: application::Model,
}

#[async_trait]
impl<B> FromRequest<B> for AuthenticatedOrganizationWithApplication
where
    B: Send,
{
    type Rejection = Error;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self> {
        let permissions = Permissions::from_request(req).await?;

        match permissions.type_ {
            KeyType::Organization => {}
            KeyType::Application => {
                return Err(HttpError::permission_denied(None, None).into());
            }
        }

        let Path(ApplicationPathParams { app_id }) =
            Path::<ApplicationPathParams>::from_request(req)
                .await
                .map_err(to_internal_server_error)?;
        let Extension(ref db) = Extension::<DatabaseConnection>::from_request(req)
            .await
            .map_err(to_internal_server_error)?;
        let app = application::Entity::secure_find_by_id_or_uid(
            permissions.org_id.clone(),
            app_id.to_owned(),
        )
        .one(db)
        .await?
        .ok_or_else(|| HttpError::not_found(None, None))?;
        Ok(AuthenticatedOrganizationWithApplication { permissions, app })
    }
}

pub struct AuthenticatedApplication {
    pub permissions: Permissions,
    pub app: application::Model,
}

#[async_trait]
impl<B> FromRequest<B> for AuthenticatedApplication
where
    B: Send,
{
    type Rejection = Error;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self> {
        let permissions = Permissions::from_request(req).await?;
        let Path(ApplicationPathParams { app_id }) =
            Path::<ApplicationPathParams>::from_request(req)
                .await
                .map_err(to_internal_server_error)?;
        let Extension(ref db) = Extension::<DatabaseConnection>::from_request(req)
            .await
            .map_err(to_internal_server_error)?;
        let app = application::Entity::secure_find_by_id_or_uid(
            permissions.org_id.clone(),
            app_id.to_owned(),
        )
        .one(db)
        .await?
        .ok_or_else(|| HttpError::not_found(None, None))?;

        if let Some(permitted_app_id) = &permissions.app_id {
            if permitted_app_id != &app.id {
                return Err(HttpError::not_found(None, None).into());
            }
        }

        Ok(AuthenticatedApplication { permissions, app })
    }
}

const JWT_ISSUER: &str = env!("CARGO_PKG_NAME");

pub fn generate_org_token(keys: &Keys, org_id: OrganizationId) -> Result<String> {
    let claims = Claims::with_custom_claims(
        CustomClaim { organization: None },
        Duration::from_hours(24 * 365 * 10),
    )
    .with_issuer(JWT_ISSUER)
    .with_subject(org_id.0);
    Ok(keys.key.authenticate(claims).unwrap())
}

pub fn generate_management_token(keys: &Keys) -> Result<String> {
    let claims =
        Claims::with_custom_claims(CustomClaim { organization: None }, Duration::from_mins(10))
            .with_issuer(JWT_ISSUER)
            .with_subject(management_org_id());
    Ok(keys.key.authenticate(claims).unwrap())
}

pub fn generate_app_token(
    keys: &Keys,
    org_id: OrganizationId,
    app_id: ApplicationId,
) -> Result<String> {
    let claims = Claims::with_custom_claims(
        CustomClaim {
            organization: Some(org_id.0),
        },
        Duration::from_hours(24 * 28),
    )
    .with_issuer(JWT_ISSUER)
    .with_subject(app_id.0);
    Ok(keys.key.authenticate(claims).unwrap())
}

#[derive(Clone, Debug)]
pub struct Keys {
    key: HS256Key,
}

impl Keys {
    pub fn new(secret: &[u8]) -> Self {
        Self {
            key: HS256Key::from_bytes(secret),
        }
    }
}

// Asymmetric Signature keys
#[derive(Clone, Eq)]
pub struct AsymmetricKey(pub KeyPair);

impl AsymmetricKey {
    pub fn generate() -> AsymmetricKey {
        AsymmetricKey(KeyPair::from_seed(Seed::generate()))
    }

    pub fn from_slice(bytes: &[u8]) -> Result<AsymmetricKey> {
        Ok(AsymmetricKey(KeyPair::from_slice(bytes).map_err(|_| {
            Error::Generic("Failed parsing key.".to_string())
        })?))
    }

    pub fn from_base64(b64: &str) -> Result<AsymmetricKey> {
        let bytes =
            base64::decode(b64).map_err(|_| Error::Generic("Failed parsing base64".to_string()))?;

        Self::from_slice(bytes.as_slice())
    }

    pub fn pubkey(&self) -> &[u8] {
        &self.0.pk[..]
    }
}

impl Debug for AsymmetricKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "<AsymmetricKey sk=*** pk={}>",
            base64::encode(self.0.pk.as_slice())
        )
    }
}

impl PartialEq for AsymmetricKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_slice() == other.0.as_slice()
    }
}

#[derive(Clone, Debug)]
pub struct Encryption(Option<Key>);

impl Encryption {
    const NONCE_SIZE: usize = 24;

    pub fn new_noop() -> Self {
        Self(None)
    }

    pub fn new(key: [u8; 32]) -> Self {
        Self(Some(Key::from_slice(&key).to_owned()))
    }

    pub fn encrypt(&self, data: &[u8]) -> crate::error::Result<Vec<u8>> {
        if let Some(main_key) = self.0.as_ref() {
            let cipher = XChaCha20Poly1305::new(main_key);
            let nonce: [u8; Self::NONCE_SIZE] = rand::thread_rng().gen();
            let nonce = XNonce::from_slice(&nonce);
            let mut ciphertext = cipher
                .encrypt(nonce, data)
                .map_err(|_| crate::error::Error::Generic("Encryption failed".to_string()))?;
            let mut ret = nonce.to_vec();
            ret.append(&mut ciphertext);
            Ok(ret)
        } else {
            Ok(data.to_vec())
        }
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> crate::error::Result<Vec<u8>> {
        if let Some(main_key) = self.0.as_ref() {
            let cipher = XChaCha20Poly1305::new(main_key);
            let nonce = &ciphertext[..Self::NONCE_SIZE];
            let ciphertext = &ciphertext[Self::NONCE_SIZE..];
            cipher
                .decrypt(XNonce::from_slice(nonce), ciphertext)
                .map_err(|_| crate::error::Error::Generic("Encryption failed".to_string()))
        } else {
            Ok(ciphertext.to_vec())
        }
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }
}

impl Default for Encryption {
    fn default() -> Self {
        Self::new_noop()
    }
}
