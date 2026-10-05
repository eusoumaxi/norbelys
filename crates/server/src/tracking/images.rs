//! Images: files a workspace uploads to show in its mail, served to anyone at a public URL, and
//! the brand mark of the platform's own mail.
//!
//! # Where an image lives
//!
//! The file is an object at `images/<workspace>/<id>.<extension>` in the deployment's object store,
//! and the `images` row records that it exists (its format, its size), so the API can find it to
//! delete it and answer `404` for an id of another workspace. Its public URL is on the tracking
//! host, where recipients' mail clients already fetch the open pixel:
//! `<tracking origin>/images/<workspace id>/<image id>.<extension>`. The ids in it are the public
//! ones (`ws_…`, `img_…`), and the image id is a fresh UUIDv7, so a URL cannot be guessed from
//! another.
//!
//! The public route reads the object alone, never the database: an image in mail already sent
//! keeps loading while the database is unavailable. A URL's content never changes (a new upload
//! is a new id), so the route lets every cache keep it for a year; once an image is deleted, a
//! cache that already holds it may keep showing it until it expires.
//!
//! # Order of effects
//!
//! A network call never runs inside a transaction: an upload writes the object first and then
//! its row (an object whose row could not be written is deleted again, best effort; one left
//! behind is unreachable through the API and has a URL nobody was given); a deletion reads the
//! row, deletes the object, then deletes the row, so an image the API no longer lists is never
//! still served by us, and a deletion interrupted half-way is finished by repeating it.
//!
//! # The brand mark
//!
//! Every transactional email shows the Norbelys mark at its top (`rendering::frame`). Its bytes
//! are compiled into the binary and the tracking role serves them at [`EMAIL_MARK_PATH`], so
//! every deployment, a self-hosted one included, links a mark that loads, with nothing to
//! install, upload or configure, and the route reads neither the database nor the object store.
//! The version in the path is the mark's: a new mark gets a new path, so the old one can be
//! cached for a year like any image whose content never changes.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use bytes::Bytes;
use serde::Serialize;
use uuid::Uuid;

use crate::db::Database;
use crate::domain::ids::{Id, Image, Workspace, WorkspaceId};
use crate::domain::images::Format;
use crate::domain::time::Timestamp;
use crate::storage::{Storage, StorageError};

/// The caching of a public image: its URL's content never changes, so every cache may keep it
/// for a year without asking again.
pub const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Where the tracking role serves the brand mark of the platform's own mail (see the module).
pub const EMAIL_MARK_PATH: &str = "/brand/v1/email-mark.png";
/// The brand mark: a 96-pixel PNG of the mark in ink pink (`#DB2777`) on transparent, drawn from
/// the brand's geometry, shown at 24 pixels so it stays sharp on dense screens.
const EMAIL_MARK: &[u8] = include_bytes!("../../assets/email-mark.png");

/// `GET /brand/v1/email-mark.png` (the tracking role): the brand mark, to anyone, as a PNG that
/// every cache may keep for a year.
pub async fn email_mark() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE)),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        EMAIL_MARK,
    )
        .into_response()
}

/// An image as the API shows it.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ImageObject {
    pub id: Id<Image>,
    /// Where anyone fetches the image: the address to put in a message's HTML.
    pub url: String,
    /// The image's media type. New values may be added.
    #[schema(value_type = Format)]
    pub content_type: String,
    /// The file's size in bytes.
    pub size: i32,
    pub created_at: Timestamp,
}

/// Why an image operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No such image in the workspace.
    #[error("no such image")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The object key of an image of the workspace whose uuid is `workspace`.
#[must_use]
pub fn key(workspace: Uuid, image: Id<Image>, format: Format) -> String {
    format!(
        "images/{}/{}.{}",
        workspace,
        image.uuid(),
        format.extension()
    )
}

/// The public URL of an image on `origin` (the tracking host's, `https://host`).
#[must_use]
pub fn url(origin: &str, workspace: WorkspaceId, image: Id<Image>, format: Format) -> String {
    format!(
        "{}/images/{workspace}/{image}.{}",
        origin.trim_end_matches('/'),
        format.extension()
    )
}

/// The image a public URL's last two segments name: the workspace id (`ws_…`) and the file name
/// (`img_….<extension>`); `None` for anything else. The workspace only chooses which object is
/// read: it selects no tenant, and an image exists for the URL only if its object does.
#[must_use]
pub fn locate(workspace: &str, file: &str) -> Option<(Id<Workspace>, Id<Image>, Format)> {
    let workspace: Id<Workspace> = workspace.parse().ok()?;
    let (image, extension) = file.split_once('.')?;
    let image: Id<Image> = image.parse().ok()?;
    let format = Format::from_extension(extension)?;
    Some((workspace, image, format))
}

/// Stores `bytes` (already checked to be a `format` image of an accepted size) as a new image of
/// `workspace`, linked on `origin`.
///
/// # Errors
///
/// The object store or the database failed; nothing remains reachable through the API.
pub async fn create(
    db: &Database,
    storage: &Storage,
    origin: &str,
    workspace: WorkspaceId,
    format: Format,
    bytes: Bytes,
) -> Result<ImageObject, Error> {
    let image = Id::<Image>::new();
    let size = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    let key = key(workspace.uuid(), image, format);
    storage.put(&key, bytes).await?;
    let inserted = async {
        let mut tx = db.begin_in(workspace).await?;
        let created_at = sqlx::query_scalar!(
            r#"INSERT INTO images (workspace_id, id, content_type, size_bytes) VALUES ($1, $2, $3, $4)
               RETURNING created_at AS "created_at: Timestamp""#,
            workspace.uuid(),
            image.uuid(),
            format.media_type(),
            size,
        )
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(created_at)
    }
    .await;
    let created_at = match inserted {
        Ok(created_at) => created_at,
        Err(error) => {
            if let Err(cleanup) = storage.delete(&key).await {
                tracing::warn!(error = %cleanup, key, "an image without a row was left in the object store");
            }
            return Err(error.into());
        }
    };
    Ok(ImageObject {
        id: image,
        url: url(origin, workspace, image, format),
        content_type: format.media_type().to_owned(),
        size,
        created_at,
    })
}

/// Deletes an image of `workspace`: its object, then its row (see the module).
///
/// # Errors
///
/// [`Error::NotFound`] when the workspace has no such image, or the store or the database failed.
pub async fn delete(
    db: &Database,
    storage: &Storage,
    workspace: WorkspaceId,
    image: Id<Image>,
) -> Result<(), Error> {
    let mut tx = db.begin_in(workspace).await?;
    let content_type = sqlx::query_scalar!(
        "SELECT content_type FROM images WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        image.uuid(),
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    tx.commit().await?;
    if let Some(format) = Format::from_media_type(&content_type) {
        storage
            .delete(&key(workspace.uuid(), image, format))
            .await?;
    }
    let mut tx = db.begin_in(workspace).await?;
    sqlx::query!(
        "DELETE FROM images WHERE workspace_id = $1 AND id = $2",
        workspace.uuid(),
        image.uuid(),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A public URL names the workspace and the image by their public ids and reads back to the
    /// same image and object key; anything else (another resource's id, an unknown extension, no
    /// extension) names no image.
    #[test]
    fn public_urls_read_back_to_their_image() {
        let workspace = WorkspaceId::trusted(Uuid::now_v7());
        let image = Id::<Image>::new();
        let url = url("https://t.example/", workspace, image, Format::Png);
        assert_eq!(
            url,
            format!("https://t.example/images/{workspace}/{image}.png")
        );
        let mut segments = url.rsplit('/');
        let file = segments.next().unwrap();
        let named = segments.next().unwrap();
        assert_eq!(
            locate(named, file),
            Some((workspace.id(), image, Format::Png))
        );
        assert_eq!(
            key(workspace.uuid(), image, Format::Png),
            format!("images/{}/{}.png", workspace.uuid(), image.uuid())
        );
        let message = Id::<crate::domain::ids::Message>::new();
        for (named, file) in [
            (named, format!("{image}.svg")),
            (named, image.to_string()),
            (named, format!("{message}.png")),
            ("img_0", format!("{image}.png")),
        ] {
            assert_eq!(locate(named, &file), None, "{named}/{file}");
        }
    }
}
