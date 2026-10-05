//! Images uploaded for mail content: which formats are accepted, and how a file proves its format.
//!
//! An image is shown by every recipient's mail client, so only the four formats mail clients
//! display are accepted: PNG, JPEG, GIF and WebP. SVG is refused: it is a document that can carry
//! scripts, and most mail clients do not show it anyway.
//!
//! The declared `Content-Type` is not trusted alone: the file's first bytes (its signature) must
//! name the same format, so a page or a script uploaded as `image/png` is refused instead of being
//! served from our host. The signatures are the formats' own: PNG's eight bytes (ISO/IEC 15948,
//! <https://www.w3.org/TR/png-3/#5PNG-file-signature>), JPEG's start-of-image marker and the
//! marker that follows it (`FF D8 FF`), GIF's `GIF87a` or `GIF89a`, and WebP's RIFF container with
//! the form type `WEBP` (<https://developers.google.com/speed/webp/docs/riff_container>).

/// The largest image accepted, in bytes: 16 MiB, the bound of every upload.
pub const SIZE_MAX: usize = 16 << 20;

/// An accepted image format. On the wire it is its media type (`image/png`, `image/jpeg`,
/// `image/gif`, `image/webp`): what an upload's `Content-Type` names and an image's
/// `content_type` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter, utoipa::ToSchema)]
#[schema(as = ImageContentType)]
pub enum Format {
    #[schema(rename = "image/png")]
    Png,
    #[schema(rename = "image/jpeg")]
    Jpeg,
    #[schema(rename = "image/gif")]
    Gif,
    #[schema(rename = "image/webp")]
    Webp,
}

impl Format {
    /// The format a `Content-Type` names (its media type, parameters ignored, any case); `None`
    /// for any other type.
    #[must_use]
    pub fn from_media_type(content_type: &str) -> Option<Self> {
        let essence = content_type.split(';').next().unwrap_or_default().trim();
        [Self::Png, Self::Jpeg, Self::Gif, Self::Webp]
            .into_iter()
            .find(|format| essence.eq_ignore_ascii_case(format.media_type()))
    }

    /// The format a file name's extension names (`png`, `jpg`, `gif`, `webp`, as
    /// [`Format::extension`] writes them); `None` otherwise.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        [Self::Png, Self::Jpeg, Self::Gif, Self::Webp]
            .into_iter()
            .find(|format| extension == format.extension())
    }

    /// The media type, as stored (`images.content_type`) and served.
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The extension of the image's file name in its public URL and its object key.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// Whether `bytes` start with this format's signature.
    #[must_use]
    pub fn matches(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::Webp => {
                bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|form| form == b"WEBP")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator as _;

    use super::Format;

    /// The smallest file of each format that starts with its signature.
    fn sample(format: Format) -> &'static [u8] {
        match format {
            Format::Png => b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR",
            Format::Jpeg => &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10],
            Format::Gif => b"GIF89a\x01\x00\x01\x00",
            Format::Webp => b"RIFF\x24\x00\x00\x00WEBPVP8 ",
        }
    }

    /// Each format reads back from its media type (any case, with parameters) and its extension,
    /// accepts its own signature and refuses every other format's, so a file cannot pass for
    /// another type. Generated over the formats.
    #[test]
    fn each_format_proves_itself_by_its_signature() {
        for format in Format::iter() {
            assert_eq!(Format::from_media_type(format.media_type()), Some(format));
            let shouted = format!("{}; charset=binary", format.media_type().to_uppercase());
            assert_eq!(Format::from_media_type(&shouted), Some(format));
            assert_eq!(Format::from_extension(format.extension()), Some(format));
            for other in Format::iter() {
                assert_eq!(
                    format.matches(sample(other)),
                    format == other,
                    "{format:?} reading {other:?}"
                );
            }
            assert!(!format.matches(b""));
        }
    }

    /// Other media types, SVG and HTML among them, are not images we accept, and a page is not
    /// any format whatever it is declared as.
    #[test]
    fn other_types_and_contents_are_refused() {
        for media_type in [
            "image/svg+xml",
            "text/html",
            "image/bmp",
            "application/octet-stream",
            "",
        ] {
            assert_eq!(Format::from_media_type(media_type), None, "{media_type}");
        }
        assert_eq!(Format::from_extension("jpeg"), None);
        for format in Format::iter() {
            assert!(!format.matches(b"<html><script>alert(1)</script></html>"));
        }
    }
}
