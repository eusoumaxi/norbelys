//! Object storage: the one module that reads and writes objects (import uploads, import error
//! reports, exports) and hands out links to download them.
//!
//! # One API, any provider
//!
//! The store speaks the S3 API through the `object_store` crate, so the provider is
//! configuration, not code: `OBJECT_STORE_URL=s3://bucket[/prefix]` with an endpoint, a region,
//! credentials and the addressing style reaches Cloudflare R2, AWS S3, Hetzner Object Storage or
//! a self-hosted server alike. `OBJECT_STORE_URL=file:///dir` keeps objects in a local
//! directory, for development and tests, so nobody runs an S3 server on a laptop. The code uses
//! only operations every S3 provider has (put, get, list, multipart upload, presigned GET), and
//! never a bucket's lifecycle rules: what is kept and for how long is decided by our own jobs
//! (`retention.prune` for expired exports and abandoned import uploads, the archive, the erasure
//! of a workspace), so moving to another provider moves the behaviour too. The one exception is
//! the parts of a multipart upload a crash left incomplete: they are no object a job can list,
//! so the rule that aborts incomplete uploads, where the provider has one, discards them.
//!
//! # Keys
//!
//! Every key starts with the kind of data and the workspace's uuid
//! (`imports/<workspace>/<import>/source.csv`, `exports/<workspace>/<export>.csv`), so a
//! workspace's objects can be listed and deleted together, and no key is ever built from client
//! input.
//!
//! # Download links
//!
//! A link is short-lived and needs no other credential, so a browser can open it:
//!
//! - on S3 it is a presigned GET URL (AWS Signature Version 4,
//!   <https://docs.aws.amazon.com/AmazonS3/latest/userguide/ShareObjectPreSignedURL.html>),
//!   signed with the store's own credentials;
//! - on a local directory there is no server to presign for, so the api serves the file itself
//!   at `GET /files/{key}?expires=…&signature=…`, a route outside `/v1` whose signature (an HMAC
//!   of the key and the expiry under the deployment's link key) is the only credential, exactly
//!   like a presigned URL. The route answers `404` for anything else: a tampered or expired
//!   link, or a store that is not local.

use std::path::Path as FilePath;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt as _};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::signer::Signer as _;
use object_store::{ObjectStore, ObjectStoreExt as _, WriteMultipart};
use secrecy::ExposeSecret as _;
use serde::Deserialize;

use crate::config::StorageArgs;
use crate::crypto::Keys;
use crate::http::AppState;
use crate::http::extract::{Path, Query};
use crate::problem::Problem;

/// Parts of a multipart upload buffered before each is sent: S3's minimum part size.
const PART_SIZE: usize = 5 << 20;
/// Parts of one upload in flight at once, which bounds a writer's memory to about 15 MiB.
const PARTS_IN_FLIGHT: usize = 2;
/// The longest a download link may live, whoever asks.
const LINK_MAX: Duration = Duration::from_secs(7 * 86_400);

/// Why an object operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The object does not exist.
    #[error("no object at `{0}`")]
    NotFound(String),
    /// The store refused or could not be reached; retrying later may succeed.
    #[error("the object store failed: {0}")]
    Unavailable(String),
    /// The configuration cannot describe a store.
    #[error("object storage is misconfigured: {0}")]
    Config(String),
}

impl From<object_store::Error> for StorageError {
    fn from(error: object_store::Error) -> Self {
        match error {
            object_store::Error::NotFound { path, .. } => Self::NotFound(path),
            other => Self::Unavailable(other.to_string()),
        }
    }
}

impl From<StorageError> for Problem {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound(_) => Problem::not_found("file"),
            StorageError::Unavailable(_) | StorageError::Config(_) => {
                tracing::error!(error = %error, "object storage failed");
                Problem::unavailable(5)
            }
        }
    }
}

/// The deployment's object store.
#[derive(Clone, Debug)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
    /// The S3 backend, which presigns download links; `None` for a local directory.
    s3: Option<Arc<AmazonS3>>,
    /// The key prefix inside the bucket, without slashes at either end; empty for none.
    prefix: String,
}

impl Storage {
    /// The store the configuration names (see [`StorageArgs`]). Without `OBJECT_STORE_URL`, a
    /// `development` deployment gets a directory under the system's temporary directory and
    /// a warning; any other deployment is refused, so a production process never keeps
    /// customer files in a container's scratch space.
    ///
    /// # Errors
    ///
    /// The URL's scheme is neither `s3` nor `file`, an `s3://` store lacks its bucket or its
    /// credentials, or the store cannot be built.
    pub fn from_args(args: &StorageArgs, environment: &str) -> Result<Self, StorageError> {
        let Some(url) = &args.object_store_url else {
            if environment != "development" {
                return Err(StorageError::Config(
                    "OBJECT_STORE_URL is required outside development".to_owned(),
                ));
            }
            let dir = std::env::temp_dir().join("norbelys-objects");
            tracing::warn!(dir = %dir.display(), "OBJECT_STORE_URL is not set: objects are kept in a temporary directory (development only)");
            return Self::local(&dir);
        };
        match url.scheme() {
            "s3" => {
                let bucket = url
                    .host_str()
                    .filter(|bucket| !bucket.is_empty())
                    .ok_or_else(|| {
                        StorageError::Config("an s3:// store names its bucket".to_owned())
                    })?;
                let (Some(key_id), Some(secret)) =
                    (&args.aws_access_key_id, &args.aws_secret_access_key)
                else {
                    return Err(StorageError::Config(
                        "an s3:// store needs AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY"
                            .to_owned(),
                    ));
                };
                let mut builder = AmazonS3Builder::new()
                    .with_bucket_name(bucket)
                    .with_region(&args.aws_region)
                    .with_access_key_id(key_id)
                    .with_secret_access_key(secret.expose_secret())
                    .with_virtual_hosted_style_request(args.aws_virtual_hosted_style_request)
                    .with_allow_http(args.aws_allow_http);
                if let Some(endpoint) = &args.aws_endpoint_url {
                    builder = builder.with_endpoint(endpoint);
                }
                let s3 = Arc::new(builder.build()?);
                Ok(Self {
                    store: s3.clone(),
                    s3: Some(s3),
                    prefix: url.path().trim_matches('/').to_owned(),
                })
            }
            "file" => {
                let dir = url.to_file_path().map_err(|()| {
                    StorageError::Config("a file:// store names an absolute directory".to_owned())
                })?;
                Self::local(&dir)
            }
            other => Err(StorageError::Config(format!(
                "`{other}://` is not a store: use s3:// or file://"
            ))),
        }
    }

    /// A store in the local directory `dir`, created if missing: development and tests.
    ///
    /// # Errors
    ///
    /// The directory cannot be created or opened.
    pub fn local(dir: &FilePath) -> Result<Self, StorageError> {
        std::fs::create_dir_all(dir).map_err(|error| {
            StorageError::Config(format!("cannot create {}: {error}", dir.display()))
        })?;
        Ok(Self {
            store: Arc::new(LocalFileSystem::new_with_prefix(dir)?),
            s3: None,
            prefix: String::new(),
        })
    }

    /// The store's location of `key`.
    fn path(&self, key: &str) -> Result<ObjectPath, StorageError> {
        let full = if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        };
        ObjectPath::parse(&full)
            .map_err(|error| StorageError::Config(format!("`{key}` is not an object key: {error}")))
    }

    /// Writes `bytes` at `key` in one request, replacing any object there: repeating a put is
    /// harmless, which is what makes a retried job safe.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub async fn put(&self, key: &str, bytes: Bytes) -> Result<(), StorageError> {
        self.store.put(&self.path(key)?, bytes.into()).await?;
        Ok(())
    }

    /// Reads the whole object at `key`. Callers read only objects whose size they bounded when
    /// they wrote them.
    ///
    /// # Errors
    ///
    /// [`StorageError::NotFound`], or the store failed.
    pub async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        Ok(self.store.get(&self.path(key)?).await?.bytes().await?)
    }

    /// The object at `key` as a response body that streams it from the store, with its size in
    /// bytes: a large object is served without being held whole in memory.
    ///
    /// # Errors
    ///
    /// [`StorageError::NotFound`], or the store failed.
    pub async fn body(&self, key: &str) -> Result<(u64, Body), StorageError> {
        let object = self.store.get(&self.path(key)?).await?;
        let size = object.meta.size;
        let stream = object.into_stream().map_err(std::io::Error::other);
        Ok((size, Body::from_stream(stream.boxed())))
    }

    /// Deletes the object at `key`; an absent object is already deleted.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self.store.delete(&self.path(key)?).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// The keys under `prefix` (a whole segment: `imports/<ws>/<id>/errors`), sorted.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        let mut keys: Vec<String> = self.stream_keys(prefix)?.try_collect().await?;
        keys.sort_unstable();
        Ok(keys)
    }

    /// Streams keys without collecting the bucket in memory. Provider order is unspecified.
    ///
    /// # Errors
    ///
    /// The prefix is invalid, or a page of the remote listing fails.
    pub fn stream_keys(
        &self,
        prefix: &str,
    ) -> Result<futures_util::stream::BoxStream<'_, Result<String, StorageError>>, StorageError>
    {
        let base = self.path(prefix)?;
        let skip = if self.prefix.is_empty() {
            0
        } else {
            self.prefix.len() + 1
        };
        Ok(self
            .store
            .list(Some(&base))
            .map_ok(move |meta| {
                meta.location
                    .as_ref()
                    .get(skip..)
                    .unwrap_or_default()
                    .to_owned()
            })
            .map_err(StorageError::from)
            .boxed())
    }

    /// Starts writing a large object at `key` as a multipart upload. Nothing is visible at
    /// `key` until [`Writer::finish`]; an abandoned upload leaves only parts, which the
    /// bucket's rule for incomplete multipart uploads removes.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub async fn writer(&self, key: &str) -> Result<Writer, StorageError> {
        let upload = self.store.put_multipart(&self.path(key)?).await?;
        Ok(Writer {
            inner: WriteMultipart::new_with_chunk_size(upload, PART_SIZE),
        })
    }

    /// A link that downloads `key` without any other credential until `ttl` has passed (at
    /// most seven days): a presigned GET on S3, or a signed link to this api's `/files` route
    /// on a local directory, under `public_api_url`.
    ///
    /// # Errors
    ///
    /// The store could not sign the link.
    pub async fn download_url(
        &self,
        keys: &Keys,
        public_api_url: &url::Url,
        key: &str,
        ttl: Duration,
    ) -> Result<url::Url, StorageError> {
        let ttl = ttl.min(LINK_MAX);
        if let Some(s3) = &self.s3 {
            return Ok(s3
                .signed_url(http::Method::GET, &self.path(key)?, ttl)
                .await?);
        }
        let expires = crate::process::now().plus(ttl).0.as_second();
        let mut url = public_api_url
            .join(&format!("files/{key}"))
            .map_err(|error| StorageError::Config(format!("a link for `{key}`: {error}")))?;
        url.query_pairs_mut()
            .append_pair("expires", &expires.to_string())
            .append_pair("signature", &link_signature(keys, key, expires));
        Ok(url)
    }

    /// True for a local directory, whose links this api serves itself.
    fn is_local(&self) -> bool {
        self.s3.is_none()
    }
}

/// What a download link needs, held together by the api's read paths: the store, the
/// deployment's keys and the api's public origin.
#[derive(Clone, Copy)]
pub struct Links<'a> {
    pub storage: &'a Storage,
    pub keys: &'a Keys,
    pub public_api_url: &'a url::Url,
}

impl Links<'_> {
    /// A link to `key` valid for `ttl` (see [`Storage::download_url`]).
    ///
    /// # Errors
    ///
    /// The store could not sign the link.
    pub async fn url(&self, key: &str, ttl: Duration) -> Result<url::Url, StorageError> {
        self.storage
            .download_url(self.keys, self.public_api_url, key, ttl)
            .await
    }
}

/// The signature of a local download link: an HMAC of the key and the expiry under the
/// deployment's link key, base64url.
fn link_signature(keys: &Keys, key: &str, expires: i64) -> String {
    URL_SAFE_NO_PAD.encode(keys.sign_link(link_payload(key, expires).as_bytes()))
}

fn link_payload(key: &str, expires: i64) -> String {
    format!("files\n{key}\n{expires}")
}

/// True when `signature` is this deployment's signature of `key` until `expires`, and `now`
/// (unix seconds) is before `expires`.
fn link_valid(keys: &Keys, key: &str, expires: i64, signature: &str, now: i64) -> bool {
    let Ok(tag) = URL_SAFE_NO_PAD.decode(signature) else {
        return false;
    };
    now < expires && keys.verify_link(link_payload(key, expires).as_bytes(), &tag)
}

/// A multipart upload being written; see [`Storage::writer`].
pub struct Writer {
    inner: WriteMultipart,
}

impl Writer {
    /// Appends `bytes`, sending each full part as it fills while at most two parts are in
    /// flight, so a writer never holds more than a few parts in memory.
    ///
    /// # Errors
    ///
    /// A part could not be uploaded.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.inner.write(bytes);
        self.inner.wait_for_capacity(PARTS_IN_FLIGHT).await?;
        Ok(())
    }

    /// Sends the last part and completes the upload: the object appears at its key whole.
    ///
    /// # Errors
    ///
    /// The upload could not be completed.
    pub async fn finish(self) -> Result<(), StorageError> {
        self.inner.finish().await?;
        Ok(())
    }

    /// Abandons the upload and asks the store to discard its parts.
    pub async fn abort(self) {
        if let Err(error) = self.inner.abort().await {
            tracing::warn!(error = %error, "an abandoned upload was not discarded");
        }
    }
}

/// The query of a local download link.
#[derive(Debug, Deserialize)]
pub struct Link {
    expires: i64,
    signature: String,
}

/// `GET /files/{key}`: serves an object of a local store to the holder of a valid link, as a
/// presigned URL would on S3. Any refusal is `404`, so a link reveals nothing about what exists.
pub async fn serve_local(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(link): Query<Link>,
) -> Response {
    let now = crate::process::now().0.as_second();
    if !state.storage.is_local()
        || !link_valid(&state.keys, &key, link.expires, &link.signature, now)
    {
        return Problem::not_found("file").into_response();
    }
    let path = match state.storage.path(&key) {
        Ok(path) => path,
        Err(_) => return Problem::not_found("file").into_response(),
    };
    let object = match state.storage.store.get(&path).await {
        Ok(object) => object,
        Err(error) => return Problem::from(StorageError::from(error)).into_response(),
    };
    let stream = object.into_stream().map_err(std::io::Error::other);
    let filename = key.rsplit('/').next().unwrap_or("download");
    let mut response = (StatusCode::OK, Body::from_stream(stream.boxed())).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(filename)),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// The media type of a file we write, by its extension.
fn content_type(filename: &str) -> &'static str {
    match filename.rsplit_once('.').map(|(_, extension)| extension) {
        Some("csv") => "text/csv; charset=utf-8",
        Some("jsonl") => "application/x-ndjson",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use object_store::ObjectStoreExt as _;
    use reqwest::StatusCode;
    use reqwest::header::RANGE;
    use uuid::Uuid;

    use super::{PART_SIZE, Storage, StorageError, link_signature, link_valid};
    use crate::testing;

    /// A local download link is valid for its key until its expiry and for nothing else: another
    /// key, a later expiry, an altered signature or a clock past the expiry are all refused, so
    /// the link can stand in for a presigned URL.
    #[test]
    fn a_local_link_is_bound_to_its_key_and_expiry() {
        let keys = testing::keys();
        let key = "exports/ws/exp.csv";
        let signature = link_signature(&keys, key, 1_000);
        assert!(link_valid(&keys, key, 1_000, &signature, 999));
        assert!(!link_valid(&keys, key, 1_000, &signature, 1_000));
        assert!(!link_valid(
            &keys,
            "exports/ws/other.csv",
            1_000,
            &signature,
            999
        ));
        assert!(!link_valid(&keys, key, 2_000, &signature, 999));
        assert!(!link_valid(&keys, key, 1_000, "AAAA", 999));
    }

    /// On request, against the real bucket the deployment's variables name, through the store as
    /// the roles build it: every operation the product relies on works there. A put reads back
    /// whole, and a ranged get returns exactly its bytes (a Parquet reader asks for a file's
    /// footer that way); a multipart upload of two parts appears whole with its size, and a range
    /// across the parts' boundary reads back; the listing of a prefix names exactly what was
    /// written; a presigned GET downloads without any other credential, whole and by range, as a
    /// browser downloads an export; deleted objects are gone. A provider missing one of these is
    /// found here, before anyone relies on it. Everything is written under `smoke/<run>/`; a
    /// failed run may leave its objects there.
    #[tokio::test]
    #[ignore = "writes to and deletes from the S3 bucket OBJECT_STORE_URL names, with the AWS_* variables: run with `cargo xtask storage smoke`"]
    async fn the_configured_bucket_does_what_the_product_needs() {
        let args = crate::config::test_storage_args().unwrap();
        assert!(
            args.object_store_url
                .as_ref()
                .is_some_and(|url| url.scheme() == "s3"),
            "OBJECT_STORE_URL names no s3:// bucket"
        );
        let storage = Storage::from_args(&args, "production").unwrap();
        let run = format!("smoke/{}", Uuid::now_v7().simple());
        let small = format!("{run}/small.txt");
        let large = format!("{run}/large.bin");

        let text = Bytes::from_static(b"0123456789abcdefghij");
        storage.put(&small, text.clone()).await.unwrap();
        assert_eq!(storage.get(&small).await.unwrap(), text);
        let range = storage
            .store
            .get_range(&storage.path(&small).unwrap(), 10..20)
            .await
            .unwrap();
        assert_eq!(&range[..], b"abcdefghij");

        let content: Vec<u8> = (0..PART_SIZE + 1_000)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let mut writer = storage.writer(&large).await.unwrap();
        writer.write(&content[..PART_SIZE]).await.unwrap();
        writer.write(&content[PART_SIZE..]).await.unwrap();
        writer.finish().await.unwrap();
        let location = storage.path(&large).unwrap();
        assert_eq!(
            storage.store.head(&location).await.unwrap().size,
            u64::try_from(content.len()).unwrap()
        );
        let boundary = u64::try_from(PART_SIZE).unwrap();
        let across = storage
            .store
            .get_range(&location, boundary - 10..boundary + 10)
            .await
            .unwrap();
        assert_eq!(&across[..], &content[PART_SIZE - 10..PART_SIZE + 10]);

        assert_eq!(
            storage.list(&run).await.unwrap(),
            [large.clone(), small.clone()]
        );

        let public_api_url = url::Url::parse("https://api.norbelys.test/").unwrap();
        let link = storage
            .download_url(
                &testing::keys(),
                &public_api_url,
                &small,
                Duration::from_secs(300),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let whole = client.get(link.clone()).send().await.unwrap();
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(whole.bytes().await.unwrap(), text);
        let part = client
            .get(link)
            .header(RANGE, "bytes=0-9")
            .send()
            .await
            .unwrap();
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(&part.bytes().await.unwrap()[..], b"0123456789");

        storage.delete(&small).await.unwrap();
        storage.delete(&large).await.unwrap();
        assert!(storage.list(&run).await.unwrap().is_empty());
        assert!(matches!(
            storage.get(&small).await,
            Err(StorageError::NotFound(_))
        ));
    }
}
