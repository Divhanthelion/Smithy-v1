//! Converting between filesystem paths and `file:` URIs.
//!
//! One implementation, because there were two and both were wrong in the same
//! way: they built `format!("file://{path}")` and never percent-encoded
//! anything. A project at `/Users/rj/Desktop/terminal empire` therefore produced
//!
//! ```text
//! file:///Users/rj/Desktop/terminal empire
//! ```
//!
//! which `Uri::from_str` rejects with "unexpected character at index 33" —
//! index 33 being the space. The language server never started, the reader hit
//! EOF, the writer hit a broken pipe, and the restart loop failed the same way
//! three times. **Any project path containing a space disabled the LSP
//! entirely**, with no message anywhere near the user saying so.
//!
//! The round trip matters as much as the encoding: diagnostics come back as
//! URIs and are matched against open buffers by path, so an encoder without a
//! matching decoder trades a broken server for diagnostics attached to a file
//! called `terminal%20empire`.

use std::path::{Path, PathBuf};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters that must not appear literally in a URI path.
///
/// Deliberately conservative — encoding more than strictly necessary is always
/// safe, since any decoder must handle it, whereas encoding too little produces
/// exactly the failure above. `/` is absent because it is the path separator and
/// must survive.
const PATH_UNSAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'[')
    .add(b']')
    .add(b'|')
    .add(b'\\')
    .add(b'^')
    .add(b'%');

/// Render a path as a `file:` URI.
pub fn path_to_uri(path: &Path) -> String {
    let path_str = path.to_string_lossy();

    #[cfg(windows)]
    let normalised = path_str.replace('\\', "/");
    #[cfg(not(windows))]
    let normalised = path_str.to_string();

    let encoded = utf8_percent_encode(&normalised, PATH_UNSAFE).to_string();

    // Windows paths start `C:/…` with no leading slash, so the authority-empty
    // form needs three slashes; POSIX paths already begin with one.
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// Recover a path from a `file:` URI, undoing [`path_to_uri`].
///
/// Returns `None` for anything that is not a `file:` URI rather than guessing —
/// a language server that reports diagnostics against `untitled:` or a scheme we
/// do not understand should be ignored, not misfiled onto a path.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Skip an authority if one is present (`file://host/path`); an empty
    // authority is the normal case and leaves `rest` starting at the path.
    let path_part = match rest.find('/') {
        Some(0) => rest,
        Some(idx) => &rest[idx..],
        None => return None,
    };

    let decoded = percent_decode_str(path_part).decode_utf8().ok()?;

    #[cfg(windows)]
    {
        // `/C:/Users/…` → `C:\Users\…`
        let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
        Some(PathBuf::from(trimmed.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(decoded.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for. `/Users/rj/Desktop/terminal empire`
    /// produced a URI containing a literal space, which the URI parser rejected
    /// at index 33 — so the language server never started for any project whose
    /// path had a space in it.
    #[test]
    fn a_path_containing_a_space_produces_a_parseable_uri() {
        let uri = path_to_uri(Path::new("/Users/rj/Desktop/terminal empire"));

        assert!(
            !uri.contains(' '),
            "a literal space is what the URI parser rejected: {uri}"
        );
        assert_eq!(uri, "file:///Users/rj/Desktop/terminal%20empire");
    }

    /// Encoding without decoding would attach diagnostics to a file called
    /// `terminal%20empire`, which matches no open buffer.
    #[test]
    fn a_path_survives_the_round_trip_through_a_uri() {
        for original in [
            "/Users/rj/Desktop/terminal empire/src/main.rs",
            "/plain/path/lib.rs",
            "/has/a#hash/and space/x.rs",
            "/percent %20 already/y.rs",
            "/unicode/ünïcødé/日本語.rs",
            "/trailing/space /z.rs",
        ] {
            let uri = path_to_uri(Path::new(original));
            assert!(
                !uri.contains(' '),
                "{original} still encodes to a URI with a space: {uri}"
            );
            assert_eq!(
                uri_to_path(&uri).as_deref(),
                Some(Path::new(original)),
                "round trip lost information for {original} (via {uri})"
            );
        }
    }

    /// A literal `%` in a filename must not be read back as the start of an
    /// escape — this is why `%` is in the unsafe set.
    #[test]
    fn a_literal_percent_in_a_filename_is_not_mistaken_for_an_escape() {
        let path = Path::new("/tmp/100%25 done/report.rs");
        let uri = path_to_uri(path);

        assert!(
            uri.contains("%2525"),
            "the percent itself is encoded: {uri}"
        );
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    /// Schemes other than `file:` are ignored rather than coerced into a path.
    #[test]
    fn a_non_file_uri_is_refused_rather_than_guessed_at() {
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
        assert_eq!(uri_to_path("https://example.com/x.rs"), None);
        assert_eq!(uri_to_path("not a uri at all"), None);
    }

    /// The form language servers actually send: empty authority, absolute path.
    #[test]
    fn an_empty_authority_is_the_normal_case() {
        assert_eq!(
            uri_to_path("file:///src/main.rs").as_deref(),
            Some(Path::new("/src/main.rs"))
        );
    }
}
