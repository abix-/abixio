use s3s::S3Result;
use s3s::auth::{S3Auth, SecretKey};
use s3s::s3_error;

pub struct AbixioAuth {
    access_key: String,
    secret_key: String,
}

impl AbixioAuth {
    pub fn new(access_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
        }
    }
}

#[async_trait::async_trait]
impl S3Auth for AbixioAuth {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        if access_key == self.access_key {
            Ok(SecretKey::from(self.secret_key.clone()))
        } else {
            Err(s3_error!(InvalidAccessKeyId))
        }
    }
}
