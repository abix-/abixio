use quick_xml::se::to_string as xml_to_string;
use serde::Serialize;

use crate::storage::StorageError;

#[derive(Debug, Clone)]
pub struct S3Error {
    pub code: &'static str,
    pub message: &'static str,
    pub http_status: u16,
}

pub const ERR_NO_SUCH_BUCKET: S3Error = S3Error {
    code: "NoSuchBucket",
    message: "The specified bucket does not exist",
    http_status: 404,
};

pub const ERR_NO_SUCH_KEY: S3Error = S3Error {
    code: "NoSuchKey",
    message: "The specified key does not exist",
    http_status: 404,
};

pub const ERR_BUCKET_EXISTS: S3Error = S3Error {
    code: "BucketAlreadyOwnedByYou",
    message: "Your previous request to create the named bucket succeeded",
    http_status: 409,
};

pub const ERR_INCOMPLETE_BODY: S3Error = S3Error {
    code: "IncompleteBody",
    message: "You did not provide the number of bytes specified",
    http_status: 400,
};

pub const ERR_INTERNAL: S3Error = S3Error {
    code: "InternalError",
    message: "We encountered an internal error",
    http_status: 500,
};

pub const ERR_METHOD_NOT_ALLOWED: S3Error = S3Error {
    code: "MethodNotAllowed",
    message: "The specified method is not allowed",
    http_status: 405,
};

pub const ERR_BUCKET_NOT_EMPTY: S3Error = S3Error {
    code: "BucketNotEmpty",
    message: "The bucket you tried to delete is not empty",
    http_status: 409,
};

pub const ERR_ACCESS_DENIED: S3Error = S3Error {
    code: "AccessDenied",
    message: "Access Denied",
    http_status: 403,
};

#[derive(Debug, Serialize)]
#[serde(rename = "Error")]
pub struct ErrorResponse {
    #[serde(rename = "Code")]
    pub code: String,
    #[serde(rename = "Message")]
    pub message: String,
}

pub fn error_to_xml(err: &S3Error) -> String {
    let resp = ErrorResponse {
        code: err.code.to_string(),
        message: err.message.to_string(),
    };
    // quick-xml serialize
    xml_to_string(&resp).unwrap_or_else(|_| {
        format!(
            "<Error><Code>{}</Code><Message>{}</Message></Error>",
            err.code, err.message
        )
    })
}

pub fn map_error(err: &StorageError) -> S3Error {
    match err {
        StorageError::BucketNotFound => ERR_NO_SUCH_BUCKET,
        StorageError::ObjectNotFound => ERR_NO_SUCH_KEY,
        StorageError::BucketExists => ERR_BUCKET_EXISTS,
        StorageError::BucketNotEmpty => ERR_BUCKET_NOT_EMPTY,
        StorageError::WriteQuorum => ERR_INTERNAL,
        StorageError::ReadQuorum => ERR_INTERNAL,
        StorageError::Bitrot => ERR_INTERNAL,
        StorageError::InvalidConfig(_) => ERR_INTERNAL,
        StorageError::Io(_) => ERR_INTERNAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_to_xml_produces_valid_xml() {
        let xml = error_to_xml(&ERR_NO_SUCH_BUCKET);
        assert!(xml.contains("NoSuchBucket"));
        assert!(xml.contains("The specified bucket does not exist"));
    }

    #[test]
    fn map_error_bucket_not_found() {
        let s3err = map_error(&StorageError::BucketNotFound);
        assert_eq!(s3err.code, "NoSuchBucket");
        assert_eq!(s3err.http_status, 404);
    }

    #[test]
    fn map_error_object_not_found() {
        let s3err = map_error(&StorageError::ObjectNotFound);
        assert_eq!(s3err.code, "NoSuchKey");
        assert_eq!(s3err.http_status, 404);
    }

    #[test]
    fn map_error_bucket_exists() {
        let s3err = map_error(&StorageError::BucketExists);
        assert_eq!(s3err.code, "BucketAlreadyOwnedByYou");
        assert_eq!(s3err.http_status, 409);
    }

    #[test]
    fn map_error_write_quorum() {
        let s3err = map_error(&StorageError::WriteQuorum);
        assert_eq!(s3err.code, "InternalError");
        assert_eq!(s3err.http_status, 500);
    }

    #[test]
    fn map_error_read_quorum() {
        let s3err = map_error(&StorageError::ReadQuorum);
        assert_eq!(s3err.code, "InternalError");
        assert_eq!(s3err.http_status, 500);
    }
}
