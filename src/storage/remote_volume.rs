use super::metadata::{BucketSettings, ObjectMeta};
use super::{Backend, BackendInfo, StorageError};
use super::internode_auth;

pub struct RemoteVolume {
    endpoint: String,
    volume_path: String,
    client: reqwest::blocking::Client,
    access_key: String,
    secret_key: String,
    no_auth: bool,
}

impl RemoteVolume {
    pub fn new(
        endpoint: String,
        volume_path: String,
        access_key: String,
        secret_key: String,
        no_auth: bool,
    ) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("build http client");
        Self { endpoint, volume_path, client, access_key, secret_key, no_auth }
    }

    fn url(&self, method: &str) -> String {
        format!("{}/_storage/v1{}", self.endpoint.trim_end_matches('/'), method)
    }

    fn get(&self, method: &str) -> reqwest::blocking::RequestBuilder {
        let mut req = self.client.get(self.url(method))
            .header("x-abixio-volume-path", &self.volume_path);
        if !self.no_auth {
            if let Ok(token) = internode_auth::sign_token(&self.access_key, &self.secret_key) {
                req = req.header("authorization", format!("Bearer {}", token));
            }
            req = req.header("x-abixio-time", internode_auth::current_time_nanos());
        }
        req
    }

    fn post(&self, method: &str) -> reqwest::blocking::RequestBuilder {
        let mut req = self.client.post(self.url(method))
            .header("x-abixio-volume-path", &self.volume_path);
        if !self.no_auth {
            if let Ok(token) = internode_auth::sign_token(&self.access_key, &self.secret_key) {
                req = req.header("authorization", format!("Bearer {}", token));
            }
            req = req.header("x-abixio-time", internode_auth::current_time_nanos());
        }
        req
    }

    fn parse_error(resp: reqwest::blocking::Response) -> StorageError {
        let status = resp.status().as_u16();
        let body = resp.text().unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or(body);
        match status {
            404 if msg.contains("bucket") => StorageError::BucketNotFound,
            404 => StorageError::ObjectNotFound,
            400 if msg.contains("invalid bucket name") => StorageError::InvalidBucketName(msg),
            400 if msg.contains("invalid object key") => StorageError::InvalidObjectKey(msg),
            400 if msg.contains("invalid version id") => StorageError::InvalidVersionId(msg),
            400 if msg.contains("invalid upload id") => StorageError::InvalidUploadId(msg),
            409 if msg.contains("not empty") => StorageError::BucketNotEmpty,
            409 => StorageError::BucketExists,
            _ => StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg)),
        }
    }
}

impl Backend for RemoteVolume {
    fn write_shard(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let meta_json = serde_json::to_string(meta)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let resp = self.post("/write-shard")
            .query(&[("bucket", bucket), ("key", key), ("meta", &meta_json)])
            .body(data.to_vec())
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Self::parse_error(resp))
        }
    }

    fn read_shard(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let resp = self.get("/read-shard")
            .query(&[("bucket", bucket), ("key", key)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() {
            return Err(Self::parse_error(resp));
        }
        let meta_json = resp.headers().get("x-abixio-meta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("{}");
        let meta: ObjectMeta = serde_json::from_str(meta_json)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let data = resp.bytes()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .to_vec();
        Ok((data, meta))
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let resp = self.post("/delete-object")
            .query(&[("bucket", bucket), ("key", key)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, StorageError> {
        let resp = self.get("/list-objects")
            .query(&[("bucket", bucket), ("prefix", prefix)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() { return Err(Self::parse_error(resp)); }
        resp.json().map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let resp = self.get("/list-buckets")
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() { return Err(Self::parse_error(resp)); }
        resp.json().map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    fn make_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let resp = self.post("/make-bucket")
            .query(&[("bucket", bucket)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let resp = self.post("/delete-bucket")
            .query(&[("bucket", bucket)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn bucket_exists(&self, bucket: &str) -> bool {
        self.get("/bucket-exists")
            .query(&[("bucket", bucket)])
            .send()
            .ok()
            .and_then(|r: reqwest::blocking::Response| r.json::<bool>().ok())
            .unwrap_or(false)
    }

    fn bucket_created_at(&self, bucket: &str) -> u64 {
        self.get("/bucket-created-at")
            .query(&[("bucket", bucket)])
            .send()
            .ok()
            .and_then(|r: reqwest::blocking::Response| r.json::<u64>().ok())
            .unwrap_or(0)
    }

    fn stat_object(&self, bucket: &str, key: &str) -> Result<ObjectMeta, StorageError> {
        let resp = self.get("/stat-object")
            .query(&[("bucket", bucket), ("key", key)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() { return Err(Self::parse_error(resp)); }
        resp.json().map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    fn update_meta(&self, bucket: &str, key: &str, meta: &ObjectMeta) -> Result<(), StorageError> {
        let resp = self.post("/update-meta")
            .query(&[("bucket", bucket), ("key", key)])
            .json(meta)
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn read_meta_versions(&self, bucket: &str, key: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        let resp = self.get("/read-meta-versions")
            .query(&[("bucket", bucket), ("key", key)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() { return Err(Self::parse_error(resp)); }
        resp.json().map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))
    }

    fn write_meta_versions(
        &self,
        bucket: &str,
        key: &str,
        versions: &[ObjectMeta],
    ) -> Result<(), StorageError> {
        let resp = self.post("/write-meta-versions")
            .query(&[("bucket", bucket), ("key", key)])
            .json(versions)
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn write_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        data: &[u8],
        meta: &ObjectMeta,
    ) -> Result<(), StorageError> {
        let meta_json = serde_json::to_string(meta)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let resp = self.post("/write-versioned-shard")
            .query(&[("bucket", bucket), ("key", key), ("version_id", version_id), ("meta", &meta_json)])
            .body(data.to_vec())
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn read_versioned_shard(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(Vec<u8>, ObjectMeta), StorageError> {
        let resp = self.get("/read-versioned-shard")
            .query(&[("bucket", bucket), ("key", key), ("version_id", version_id)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if !resp.status().is_success() { return Err(Self::parse_error(resp)); }
        let meta_json = resp.headers().get("x-abixio-meta")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("{}");
        let meta: ObjectMeta = serde_json::from_str(meta_json)
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let data = resp.bytes()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .to_vec();
        Ok((data, meta))
    }

    fn delete_version_data(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), StorageError> {
        let resp = self.post("/delete-version-data")
            .query(&[("bucket", bucket), ("key", key), ("version_id", version_id)])
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn read_bucket_settings(&self, bucket: &str) -> BucketSettings {
        self.get("/read-bucket-settings")
            .query(&[("bucket", bucket)])
            .send()
            .ok()
            .and_then(|r: reqwest::blocking::Response| r.json::<BucketSettings>().ok())
            .unwrap_or_default()
    }

    fn write_bucket_settings(
        &self,
        bucket: &str,
        settings: &BucketSettings,
    ) -> Result<(), StorageError> {
        let resp = self.post("/write-bucket-settings")
            .query(&[("bucket", bucket)])
            .json(settings)
            .send()
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        if resp.status().is_success() { Ok(()) } else { Err(Self::parse_error(resp)) }
    }

    fn info(&self) -> BackendInfo {
        self.get("/info")
            .send()
            .ok()
            .and_then(|r: reqwest::blocking::Response| r.json::<BackendInfo>().ok())
            .unwrap_or(BackendInfo {
                label: format!("{}:{}", self.endpoint, self.volume_path),
                volume_id: String::new(),
                backend_type: "remote".to_string(),
                total_bytes: None,
                used_bytes: None,
                free_bytes: None,
            })
    }
}
