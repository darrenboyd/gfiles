//! Assigns each file a kind from its extension.
//!
//! PDFs count as documents, not books: most PDFs aren't books, and telling them apart needs
//! the file's metadata, which comes in a later milestone.

pub const REPO: &str = "repo";
pub const OTHER: &str = "other";

const KINDS: &[(&str, &[&str])] = &[
    (
        "book",
        &[
            "epub", "mobi", "azw", "azw3", "kfx", "fb2", "djvu", "cbz", "cbr", "lit",
        ],
    ),
    (
        "photo",
        &[
            "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "avif",
            "dng", "cr2", "cr3", "nef", "arw", "orf", "rw2", "raf", "srw", "pef",
        ],
    ),
    (
        "music",
        &[
            "mp3", "flac", "m4a", "aac", "ogg", "oga", "opus", "wav", "aif", "aiff", "wma", "alac",
            "ape",
        ],
    ),
    (
        "video",
        &[
            "mp4", "m4v", "mkv", "mov", "avi", "wmv", "webm", "mpg", "mpeg", "m2ts", "mts", "3gp",
            "flv",
        ],
    ),
    (
        "document",
        &[
            "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "org", "tex", "pages", "xls", "xlsx",
            "ods", "csv", "numbers", "ppt", "pptx", "odp", "key",
        ],
    ),
    (
        "archive",
        &[
            "zip", "tar", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "iso", "dmg", "img",
        ],
    ),
    (
        "code",
        &[
            "rs", "rb", "py", "js", "ts", "go", "c", "h", "cpp", "hpp", "java", "kt", "swift",
            "lua", "sh", "ex", "exs", "erb", "html", "css", "scss", "sql", "json", "yml", "yaml",
            "toml",
        ],
    ),
];

/// The kind for a lowercase extension (no dot), or `"other"`.
pub fn kind_for_ext(ext: Option<&str>) -> &'static str {
    let Some(ext) = ext else { return OTHER };
    KINDS
        .iter()
        .find(|(_, exts)| exts.contains(&ext))
        .map_or(OTHER, |(kind, _)| kind)
}

/// Every kind a search can filter on.
pub fn all_kinds() -> impl Iterator<Item = &'static str> {
    KINDS.iter().map(|(k, _)| *k).chain([REPO, OTHER])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_for_ext(Some("epub")), "book");
        assert_eq!(kind_for_ext(Some("pdf")), "document");
        assert_eq!(kind_for_ext(Some("nef")), "photo");
        assert_eq!(kind_for_ext(Some("flac")), "music");
        assert_eq!(kind_for_ext(Some("weird")), "other");
        assert_eq!(kind_for_ext(None), "other");
        assert!(all_kinds().any(|k| k == "repo"));
    }

    #[test]
    fn no_extension_in_two_kinds() {
        let mut seen = std::collections::HashSet::new();
        for (_, exts) in KINDS {
            for e in *exts {
                assert!(seen.insert(*e), "{e} listed twice");
            }
        }
    }
}
