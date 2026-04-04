use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename = "ListAllMyBucketsResult")]
pub struct ListAllMyBucketsResult {
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "Owner")]
    pub owner: Owner,
    #[serde(rename = "Buckets")]
    pub buckets: Buckets,
}

#[derive(Debug, Serialize)]
pub struct Owner {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct Buckets {
    #[serde(rename = "Bucket")]
    pub bucket: Vec<BucketXml>,
}

#[derive(Debug, Serialize)]
pub struct BucketXml {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "CreationDate")]
    pub creation_date: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "ListBucketResult")]
pub struct ListBucketResultV2 {
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Prefix")]
    pub prefix: String,
    #[serde(rename = "KeyCount")]
    pub key_count: usize,
    #[serde(rename = "MaxKeys")]
    pub max_keys: usize,
    #[serde(rename = "IsTruncated")]
    pub is_truncated: bool,
    #[serde(rename = "Contents")]
    pub contents: Vec<ObjectXml>,
    #[serde(rename = "CommonPrefixes", skip_serializing_if = "Vec::is_empty")]
    pub common_prefixes: Vec<PrefixXml>,
    #[serde(
        rename = "NextContinuationToken",
        skip_serializing_if = "String::is_empty"
    )]
    pub next_continuation_token: String,
}

#[derive(Debug, Serialize)]
pub struct ObjectXml {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "LastModified")]
    pub last_modified: String,
    #[serde(rename = "ETag")]
    pub etag: String,
    #[serde(rename = "Size")]
    pub size: u64,
    #[serde(rename = "StorageClass")]
    pub storage_class: String,
}

#[derive(Debug, Serialize)]
pub struct PrefixXml {
    #[serde(rename = "Prefix")]
    pub prefix: String,
}

// -- DeleteObjects request/response types --

#[derive(Debug, Deserialize)]
#[serde(rename = "Delete")]
pub struct DeleteRequest {
    #[serde(rename = "Object")]
    pub objects: Vec<DeleteObjectEntry>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteObjectEntry {
    #[serde(rename = "Key")]
    pub key: String,
}

#[derive(Debug, Serialize)]
#[serde(rename = "DeleteResult")]
pub struct DeleteResult {
    #[serde(rename = "@xmlns")]
    pub xmlns: String,
    #[serde(rename = "Deleted", skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<DeletedXml>,
    #[serde(rename = "Error", skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<DeleteErrorXml>,
}

#[derive(Debug, Serialize)]
pub struct DeletedXml {
    #[serde(rename = "Key")]
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteErrorXml {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
}

// -- CopyObject response --

#[derive(Debug, Serialize)]
#[serde(rename = "CopyObjectResult")]
pub struct CopyObjectResultXml {
    #[serde(rename = "ETag")]
    pub etag: String,
    #[serde(rename = "LastModified")]
    pub last_modified: String,
}

pub const S3_XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

pub fn default_owner() -> Owner {
    Owner {
        id: "abixio".to_string(),
        display_name: "abixio".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::se::to_string as xml_to_string;

    #[test]
    fn list_all_buckets_serializes() {
        let result = ListAllMyBucketsResult {
            xmlns: S3_XMLNS.to_string(),
            owner: default_owner(),
            buckets: Buckets {
                bucket: vec![BucketXml {
                    name: "test".to_string(),
                    creation_date: "2024-01-01T00:00:00.000Z".to_string(),
                }],
            },
        };
        let xml = xml_to_string(&result).unwrap();
        assert!(xml.contains("ListAllMyBucketsResult"));
        assert!(xml.contains(S3_XMLNS));
        assert!(xml.contains("test"));
    }

    #[test]
    fn list_bucket_result_v2_serializes() {
        let result = ListBucketResultV2 {
            xmlns: S3_XMLNS.to_string(),
            name: "mybucket".to_string(),
            prefix: "".to_string(),
            key_count: 1,
            max_keys: 1000,
            is_truncated: false,
            contents: vec![ObjectXml {
                key: "mykey".to_string(),
                last_modified: "2024-01-01T00:00:00.000Z".to_string(),
                etag: "\"abc123\"".to_string(),
                size: 100,
                storage_class: "STANDARD".to_string(),
            }],
            common_prefixes: vec![PrefixXml {
                prefix: "logs/".to_string(),
            }],
            next_continuation_token: String::new(),
        };
        let xml = xml_to_string(&result).unwrap();
        assert!(xml.contains("ListBucketResult"));
        assert!(xml.contains("mykey"));
        assert!(xml.contains("logs/"));
    }

    #[test]
    fn error_response_serializes() {
        use crate::s3::errors::ErrorResponse;
        let resp = ErrorResponse {
            code: "NoSuchKey".to_string(),
            message: "The specified key does not exist".to_string(),
        };
        let xml = xml_to_string(&resp).unwrap();
        assert!(xml.contains("Error"));
        assert!(xml.contains("NoSuchKey"));
    }
}
