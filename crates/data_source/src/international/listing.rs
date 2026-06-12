//! HTML directory-listing ("autoindex") parser shared by the international
//! providers whose catalogs are plain web-server indexes.
//!
//! Both index dialects used by the European open-data radar feeds are
//! covered, and every fixture in `tests/fixtures/` is a real capture
//! (2026-06-12 UTC):
//!
//! - Apache `mod_autoindex` table layout (SHMU, opendata.shmu.sk): rows of
//!   `<tr><td>...<a href="skjav/">skjav/</a>...` plus `?C=N;O=D` sort links
//!   and an absolute-path parent link.
//! - nginx `autoindex` `<pre>` layout (DWD opendata.dwd.de, CHMI
//!   opendata.chmi.cz): `<a href="name">truncated text..&gt;</a>  date  size`
//!   lines plus a `../` parent link. The anchor TEXT is truncated for long
//!   names — only the `href` attribute is trustworthy, which is why this
//!   parser never reads element text.
//!
//! The parser extracts `href` values, drops navigation noise (sort links,
//! parent links, absolute URLs/paths), and reports each surviving entry as a
//! file or directory by its trailing slash. Entry names are kept verbatim
//! (HTML entities unescaped, but no percent-decoding) so they can be joined
//! back onto the listing URL for fetching.

/// One file or subdirectory in a parsed index page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListingEntry {
    /// Entry name relative to the listing's directory, without the trailing
    /// slash for directories. Verbatim from the `href` attribute (entities
    /// unescaped), so joining it back onto the listing URL re-creates the
    /// fetchable URL.
    pub name: String,
    /// `true` when the href ended with `/` (a subdirectory).
    pub is_dir: bool,
}

/// Extract the file/directory entries from an Apache/nginx autoindex page.
///
/// Tolerant by construction: anything that is not a relative single-segment
/// href is skipped (sort links like `?C=N;O=D`, parent links `../` or
/// absolute paths, full URLs), and malformed attributes simply contribute
/// nothing. Never panics on arbitrary input.
pub fn parse_autoindex(html: &str) -> Vec<ListingEntry> {
    let mut entries = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("href=") {
        rest = &rest[at + "href=".len()..];
        let Some(raw) = take_attribute_value(&mut rest) else {
            continue;
        };
        let name = unescape_html_entities(raw);
        if let Some(entry) = entry_from_href(&name) {
            entries.push(entry);
        }
    }
    entries
}

/// `true` when the listing holds a subdirectory of exactly this name.
pub fn has_dir(entries: &[ListingEntry], name: &str) -> bool {
    entries
        .iter()
        .any(|entry| entry.is_dir && entry.name == name)
}

/// Join a directory URL and a listing entry name into a fetchable URL.
pub fn join_url(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// First run of exactly `len` consecutive ASCII digits in `name` — the
/// timestamp extractor for feed file names (14 digits for SHMU/CHMI
/// `T_PAGZ41_C_LZIB_20260612060000.hdf`, 16 for DWD
/// `...th_00-2026061206455700-asb-10103-hd5` with trailing centiseconds).
pub fn digit_run(name: &str, len: usize) -> Option<&str> {
    let bytes = name.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end - start == len {
            return Some(&name[start..end]);
        }
        start = end;
    }
    None
}

/// FNV-1a 64-bit hash (Fowler–Noll–Vo), used to keep multi-part frame
/// identities bounded: a 50-part DWD plan hashes its URL list instead of
/// embedding it. Deterministic across runs/platforms by definition.
pub fn fnv1a64(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Read one attribute value at the head of `rest` (quoted with `"` or `'`,
/// or bare up to whitespace/`>`), advancing `rest` past it.
fn take_attribute_value<'a>(rest: &mut &'a str) -> Option<&'a str> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first == '"' || first == '\'' {
        let body = &rest[first.len_utf8()..];
        let end = body.find(first)?;
        let value = &body[..end];
        *rest = &body[end + first.len_utf8()..];
        Some(value)
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        let value = &rest[..end];
        *rest = &rest[end..];
        Some(value)
    }
}

/// Classify a cleaned href into a listing entry, or `None` for navigation
/// links and anything that is not a relative single-segment path.
fn entry_from_href(href: &str) -> Option<ListingEntry> {
    if href.is_empty()
        || href.starts_with('?')
        || href.starts_with('#')
        || href.starts_with('/')
        || href.starts_with("./")
        || href.starts_with("../")
        || href.contains("://")
    {
        return None;
    }
    let (name, is_dir) = match href.strip_suffix('/') {
        Some(name) => (name, true),
        None => (href, false),
    };
    // Multi-segment hrefs (nested paths) are not entries of THIS directory.
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(ListingEntry {
        name: name.to_owned(),
        is_dir,
    })
}

/// Minimal HTML entity unescape for href attribute values: the named
/// entities autoindex emits plus numeric character references.
fn unescape_html_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..semi];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|num| {
                    num.strip_prefix(['x', 'X']).map_or_else(
                        || num.parse::<u32>().ok(),
                        |hex| u32::from_str_radix(hex, 16).ok(),
                    )
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(ch) => {
                out.push(ch);
                rest = &rest[semi + 1..];
            }
            None => {
                // Not a recognized entity: keep the ampersand literally.
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHMU_ROOT: &str = include_str!("../../tests/fixtures/shmu_volume_root.html");
    const DWD_ROOT: &str = include_str!("../../tests/fixtures/dwd_sites_root.html");
    const CHMI_ROOT: &str = include_str!("../../tests/fixtures/chmi_sites_root.html");
    const DWD_Z_FILES: &str = include_str!("../../tests/fixtures/dwd_asb_z_unfiltered_files.html");

    #[test]
    fn apache_table_listing_yields_dirs_without_navigation_noise() {
        let entries = parse_autoindex(SHMU_ROOT);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        // Sort links (?C=N;O=D) and the absolute-path parent link are gone.
        assert_eq!(names, ["metadata", "skjav", "skkoj", "skkub", "sklaz"]);
        assert!(entries.iter().all(|entry| entry.is_dir));
    }

    #[test]
    fn nginx_pre_listing_yields_dirs_without_parent_link() {
        let entries = parse_autoindex(CHMI_ROOT);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["brd", "ska"]);

        let dwd = parse_autoindex(DWD_ROOT);
        assert!(has_dir(&dwd, "sweep_vol_z"));
        assert!(has_dir(&dwd, "sweep_vol_v"));
        assert!(!dwd.iter().any(|entry| entry.name.contains("..")));
    }

    #[test]
    fn nginx_file_rows_keep_full_names_despite_truncated_link_text() {
        // The anchor text in the DWD listing is truncated
        // ("...th_00-20260610064..&gt;"); the href is not.
        let entries = parse_autoindex(DWD_Z_FILES);
        assert!(entries.iter().any(|entry| {
            entry.name == "ras07-vol5minng01_sweeph5onem_th_09-2026061206440200-asb-10103-hd5"
                && !entry.is_dir
        }));
        // 30 timestamped th + 30 tv + 20 LATEST aliases in the trimmed capture.
        assert_eq!(entries.len(), 80);
    }

    #[test]
    fn hrefs_with_entities_quotes_and_bare_values_parse() {
        let html = r#"<a href="a&amp;b.h5">x</a> <a href='dir/'>y</a> <a href=plain.hdf>z</a>"#;
        let entries = parse_autoindex(html);
        assert_eq!(
            entries,
            vec![
                ListingEntry {
                    name: "a&b.h5".to_owned(),
                    is_dir: false
                },
                ListingEntry {
                    name: "dir".to_owned(),
                    is_dir: true
                },
                ListingEntry {
                    name: "plain.hdf".to_owned(),
                    is_dir: false
                },
            ]
        );
    }

    #[test]
    fn navigation_and_nested_hrefs_are_skipped() {
        let html = concat!(
            r#"<a href="?C=N;O=D">Name</a><a href="/abs/path/">Parent</a>"#,
            r#"<a href="../">..</a><a href="https://example.invalid/x">ext</a>"#,
            r##"<a href="a/b.hdf">nested</a><a href="#frag">frag</a><a href="">empty</a>"##,
        );
        assert!(parse_autoindex(html).is_empty());
    }

    #[test]
    fn numeric_entities_and_unknown_entities_are_tolerated() {
        assert_eq!(unescape_html_entities("a&#65;&#x42;c"), "aABc");
        assert_eq!(unescape_html_entities("5&6;7"), "5&6;7");
        assert_eq!(unescape_html_entities("trailing&amp"), "trailing&amp");
    }

    #[test]
    fn digit_run_finds_exact_length_runs_only() {
        assert_eq!(
            digit_run("T_PAGZ41_C_LZIB_20260612060000.hdf", 14),
            Some("20260612060000")
        );
        assert_eq!(
            digit_run(
                "ras07-vol5minng01_sweeph5onem_th_00-2026061206455700-asb-10103-hd5",
                16
            ),
            Some("2026061206455700")
        );
        assert_eq!(digit_run("T_PAGZ41_C_LZIB_20260612060000.hdf", 16), None);
        assert_eq!(digit_run("no-digits", 14), None);
    }

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // Reference values from the FNV specification (Fowler–Noll–Vo).
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64("foobar"), 0x85944171f73967e8);
    }

    #[test]
    fn join_url_inserts_exactly_one_slash() {
        assert_eq!(join_url("https://x/y/", "z.hdf"), "https://x/y/z.hdf");
        assert_eq!(join_url("https://x/y", "z.hdf"), "https://x/y/z.hdf");
    }
}
