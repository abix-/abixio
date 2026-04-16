//! Map s3s operation identifiers to the S3 policy action strings
//! (`s3:GetObject` and friends). Only the ops abixio actually serves
//! need entries; anything not in the table gets `s3:*` as a safe
//! fallback that participates in action-wildcard matches.

/// Translate an s3s operation name (e.g. `"GetObject"` from
/// `S3Op::name()`) into its S3 policy action string. Unknown names
/// return `"s3:*"` which only matches statements that use the `s3:*`
/// wildcard.
pub fn op_to_action(op_name: &str) -> &'static str {
    match op_name {
        "AbortMultipartUpload" => "s3:AbortMultipartUpload",
        "CompleteMultipartUpload" => "s3:PutObject",
        "CopyObject" => "s3:PutObject",
        "CreateBucket" => "s3:CreateBucket",
        "CreateMultipartUpload" => "s3:PutObject",
        "DeleteBucket" => "s3:DeleteBucket",
        "DeleteBucketCors" => "s3:PutBucketCORS",
        "DeleteBucketEncryption" => "s3:PutEncryptionConfiguration",
        "DeleteBucketLifecycle" => "s3:PutLifecycleConfiguration",
        "DeleteBucketPolicy" => "s3:DeleteBucketPolicy",
        "DeleteBucketReplication" => "s3:PutReplicationConfiguration",
        "DeleteBucketTagging" => "s3:PutBucketTagging",
        "DeleteBucketWebsite" => "s3:DeleteBucketWebsite",
        "DeleteObject" => "s3:DeleteObject",
        "DeleteObjects" => "s3:DeleteObject",
        "DeleteObjectTagging" => "s3:DeleteObjectTagging",
        "GetBucketAcl" => "s3:GetBucketAcl",
        "GetBucketCors" => "s3:GetBucketCORS",
        "GetBucketEncryption" => "s3:GetEncryptionConfiguration",
        "GetBucketLifecycle" | "GetBucketLifecycleConfiguration" => "s3:GetLifecycleConfiguration",
        "GetBucketLocation" => "s3:GetBucketLocation",
        "GetBucketLogging" => "s3:GetBucketLogging",
        "GetBucketNotification" | "GetBucketNotificationConfiguration" => "s3:GetBucketNotification",
        "GetBucketPolicy" => "s3:GetBucketPolicy",
        "GetBucketReplication" => "s3:GetReplicationConfiguration",
        "GetBucketRequestPayment" => "s3:GetBucketRequestPayment",
        "GetBucketTagging" => "s3:GetBucketTagging",
        "GetBucketVersioning" => "s3:GetBucketVersioning",
        "GetBucketWebsite" => "s3:GetBucketWebsite",
        "GetObject" => "s3:GetObject",
        "GetObjectAcl" => "s3:GetObjectAcl",
        "GetObjectTagging" => "s3:GetObjectTagging",
        "GetObjectVersion" => "s3:GetObjectVersion",
        "HeadBucket" => "s3:ListBucket",
        "HeadObject" => "s3:GetObject",
        "ListBucket" => "s3:ListBucket",
        "ListBuckets" => "s3:ListAllMyBuckets",
        "ListMultipartUploads" => "s3:ListBucketMultipartUploads",
        "ListObjects" | "ListObjectsV2" => "s3:ListBucket",
        "ListObjectVersions" => "s3:ListBucketVersions",
        "ListParts" => "s3:ListMultipartUploadParts",
        "PutBucketAcl" => "s3:PutBucketAcl",
        "PutBucketCors" => "s3:PutBucketCORS",
        "PutBucketEncryption" => "s3:PutEncryptionConfiguration",
        "PutBucketLifecycle" | "PutBucketLifecycleConfiguration" => "s3:PutLifecycleConfiguration",
        "PutBucketLogging" => "s3:PutBucketLogging",
        "PutBucketNotification" | "PutBucketNotificationConfiguration" => "s3:PutBucketNotification",
        "PutBucketPolicy" => "s3:PutBucketPolicy",
        "PutBucketReplication" => "s3:PutReplicationConfiguration",
        "PutBucketRequestPayment" => "s3:PutBucketRequestPayment",
        "PutBucketTagging" => "s3:PutBucketTagging",
        "PutBucketVersioning" => "s3:PutBucketVersioning",
        "PutBucketWebsite" => "s3:PutBucketWebsite",
        "PutObject" => "s3:PutObject",
        "PutObjectAcl" => "s3:PutObjectAcl",
        "PutObjectTagging" => "s3:PutObjectTagging",
        "RestoreObject" => "s3:RestoreObject",
        "UploadPart" => "s3:PutObject",
        "UploadPartCopy" => "s3:PutObject",
        _ => "s3:*",
    }
}

#[cfg(test)]
mod tests {
    use super::op_to_action;

    #[test]
    fn common_ops_mapped() {
        assert_eq!(op_to_action("GetObject"), "s3:GetObject");
        assert_eq!(op_to_action("PutObject"), "s3:PutObject");
        assert_eq!(op_to_action("ListObjectsV2"), "s3:ListBucket");
        assert_eq!(op_to_action("HeadBucket"), "s3:ListBucket");
        assert_eq!(op_to_action("DeleteObject"), "s3:DeleteObject");
    }

    #[test]
    fn unknown_falls_back_to_wildcard() {
        assert_eq!(op_to_action("Nonesuch"), "s3:*");
    }
}
