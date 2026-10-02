//! Human-readable paths only. Canonical identities and filesystem inputs stay intact.
use regex::{Captures, Regex};
use std::{borrow::Cow, path::Path, sync::LazyLock};

pub(crate) fn display_path(path: &Path) -> String {
    display_text(&path.to_string_lossy()).into_owned()
}

pub(crate) fn display_text(text: &str) -> Cow<'_, str> {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\\\\\?\\(?:([A-Za-z]:\\)|(?i:UNC)\\([^\\\r\n]+\\[^\\\r\n]+))").unwrap()
    });
    PREFIX.replace_all(text, |captures: &Captures<'_>| {
        captures.get(1).map_or_else(
            || format!(r"\\{}", &captures[2]),
            |drive| drive.as_str().to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_preserves_namespaces_and_normalizes_embedded_drive_and_unc_paths() {
        assert_eq!(
            display_text(
                r"읽기 실패: \\?\C:\프로젝트\결과.md; 다시 확인: \\?\UNC\server\share\문서.md"
            ),
            r"읽기 실패: C:\프로젝트\결과.md; 다시 확인: \\server\share\문서.md",
        );
        assert_eq!(
            display_path(Path::new(r"\\?\unc\server\share")),
            r"\\server\share"
        );
        for path in [
            r"\\?\Volume{abc}\문서.md",
            r"\\.\C:\문서.md",
            r"\\?\C:relative",
            r"\\?\UNC\server",
            "/Users/developer/문서.md",
            "docs/result.md",
        ] {
            assert_eq!(display_path(Path::new(path)), path);
        }
    }
}
