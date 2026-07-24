#![allow(dead_code)] // public API consumed by http routes + watcher (wired by sibling modules).

//! Filesystem ↔ Roblox Instance mapping.
//!
//! Conventions (scope: scripts + folders only):
//!   * `Foo.luau`            → ModuleScript named `Foo`
//!   * `Foo.server.luau`     → Script          named `Foo`
//!   * `Foo.client.luau`     → LocalScript     named `Foo`
//!   * Wally-style `.lua`, `.server.lua`, and `.client.lua` files are accepted
//!     with the same class mapping.
//!   * `Foo/` (dir)          → Folder          named `Foo`
//!   * `Foo/init (Foo).luau` → the folder IS a ModuleScript named `Foo` whose children
//!     are the other entries in the folder. `.lua`, `.server.*`, and
//!     `.client.*` variants are accepted.
//!   * `Foo/init.lua`        → Wally/Rojo-style equivalent of `Foo/init (Foo).luau`.
//!   * Sibling name collisions are broken with numeric suffixes.
//!   * Unsafe characters in instance names are percent-encoded.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use crate::fs_safety::{metadata_no_follow, PortableDirectoryIndex, SafeEntryKind};

pub const META_FILE: &str = ".meta.json";
pub const MAX_PORTABLE_COMPONENT_BYTES: usize = 255;
pub const EMPTY_NAME_SENTINEL: &str = "%_";

/// Normalize CRLF → LF. Returns borrowed when nothing would change, owned
/// otherwise. Used before hashing / comparing script bytes so that a Studio
/// push (always LF) and an FS file checked out with CRLF on Windows don't
/// masquerade as divergent content.
pub fn normalize_line_endings(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !bytes.contains(&b'\r') {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptClass {
    ModuleScript,
    Script,
    LocalScript,
}

impl ScriptClass {
    pub fn class_name(self) -> &'static str {
        match self {
            ScriptClass::ModuleScript => "ModuleScript",
            ScriptClass::Script => "Script",
            ScriptClass::LocalScript => "LocalScript",
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            ScriptClass::ModuleScript => ".luau",
            ScriptClass::Script => ".server.luau",
            ScriptClass::LocalScript => ".client.luau",
        }
    }

    pub fn from_class(class: &str) -> Option<Self> {
        match class {
            "ModuleScript" => Some(Self::ModuleScript),
            "Script" => Some(Self::Script),
            "LocalScript" => Some(Self::LocalScript),
            _ => None,
        }
    }
}

/// Preferred source marker for a script-with-children directory.
///
/// The named Ro Sync form is easier to recognize by eye, but a near-limit
/// directory name can make `init (<Name>).server.luau` exceed Windows' 255
/// UTF-16-code-unit component limit. The already-supported plain init form
/// derives identity from the parent directory and stays portable.
pub fn portable_init_file_name(name: &str, class: ScriptClass) -> String {
    let named = format!("init ({}){}", encode_name(name), class.suffix());
    if named.len() <= MAX_PORTABLE_COMPONENT_BYTES {
        named
    } else {
        format!("init{}", class.suffix())
    }
}

/// Classify a file name as a script, returning `(class, stem_without_ext)`.
///
/// Order matters: class-specific suffixes must be tried before plain module
/// suffixes so `Foo.server.lua` does not become a ModuleScript named
/// `Foo.server`.
pub fn classify_script_file(file_name: &str) -> Option<(ScriptClass, String)> {
    const SUFFIXES: &[(&str, ScriptClass)] = &[
        (".server.luau", ScriptClass::Script),
        (".client.luau", ScriptClass::LocalScript),
        (".server.lua", ScriptClass::Script),
        (".client.lua", ScriptClass::LocalScript),
        (".luau", ScriptClass::ModuleScript),
        (".lua", ScriptClass::ModuleScript),
    ];
    for (suffix, class) in SUFFIXES {
        if let Some(stem) = file_name.strip_suffix(suffix) {
            return Some((*class, stem.to_string()));
        }
    }
    None
}

/// Parse an `init (<Name>).{server,client,}.{luau,lua}` file name. The returned name
/// is the instance's *clean* name — any `[N]` disambiguation suffix inside the
/// parentheses is stripped so the inner matches the Roblox instance name.
pub fn parse_init_file(file_name: &str) -> Option<(ScriptClass, String)> {
    let (class, stem) = classify_script_file(file_name)?;
    let inner = stem.strip_prefix("init (")?.strip_suffix(')')?;
    if inner.is_empty() {
        return None;
    }
    let encoded_name = match parse_disambiguated(inner) {
        Some((n, _)) => n,
        None => inner.to_string(),
    };
    Some((class, decode_name(&encoded_name)))
}

/// Parse a Wally/Rojo-style plain init file name.
///
/// Unlike `init (<Name>).luau`, a plain `init.lua` does not encode the parent
/// instance name. Callers that are inspecting a directory should use the
/// surrounding folder's name for the resulting instance.
pub fn parse_plain_init_file(file_name: &str) -> Option<ScriptClass> {
    let (class, stem) = classify_script_file(file_name)?;
    (stem == "init").then_some(class)
}

/// True for both Ro-Sync `init (<Name>).luau` and Wally/Rojo `init.lua`
/// variants. These files describe their parent directory and should not be
/// emitted as standalone child ModuleScripts when that parent is a script.
pub fn is_init_file(file_name: &str) -> bool {
    parse_init_file(file_name).is_some() || parse_plain_init_file(file_name).is_some()
}

/// Parse a `<Name> [N]` numeric disambiguation suffix off a stem. Returns the
/// clean name and the 1-based ordinal if present.
pub fn parse_disambiguated(stem: &str) -> Option<(String, usize)> {
    let inner = stem.strip_suffix(']')?;
    let open = inner.rfind(" [")?;
    let name = &inner[..open];
    let n_str = &inner[open + 2..];
    if name.is_empty() || n_str.is_empty() {
        return None;
    }
    let n: usize = n_str.parse().ok()?;
    Some((name.to_string(), n))
}

// ---------------------------------------------------------------------------
// Filename encoding
// ---------------------------------------------------------------------------

fn needs_escape(ch: char) -> bool {
    // Keep generated fragments ASCII-only. Literal Unicode filenames can alias
    // under APFS/HFS+ normalization and case folding (for example `É`/`é`),
    // while NTFS applies a different case table. Percent-encoding UTF-8 bytes
    // gives every platform the same bytewise identity without requiring the
    // Studio plugin to implement Unicode normalization/case folding.
    !ch.is_ascii()
        || matches!(
            ch,
            '/' | '\\' | '\0' | '%' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
        )
        || (ch as u32) < 0x20
}

/// Case-insensitive match against the Windows reserved device-name list. These
/// names (and `NAME.ext` forms) can't be created as files on Windows at all.
fn is_windows_reserved_stem(stem: &str) -> bool {
    // Reserved names only match the portion before the first dot.
    let head = stem.split('.').next().unwrap_or(stem);
    let upper = head.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "CONIN$"
            | "CONOUT$"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Percent-encode non-ASCII text and characters that can't (or shouldn't)
/// appear in POSIX / NTFS paths. ASCII-only output avoids Unicode
/// case/normalization aliases across filesystems. Also guards against leading
/// ASCII space, trailing `.`/space, and Windows reserved device names (CON,
/// PRN, ...).
pub fn encode_name(name: &str) -> String {
    // An empty path component collapses to its parent on every supported
    // filesystem. `%_` cannot be emitted for a literal Roblox name because
    // literal percent signs are encoded as `%25`, so it is a compact,
    // reversible sentinel (`"%_"` itself becomes `%25_`).
    if name.is_empty() {
        return EMPTY_NAME_SENTINEL.to_string();
    }

    let mut out = String::with_capacity(name.len());
    let char_count = name.chars().count();
    for (i, ch) in name.chars().enumerate() {
        let leading_dot = i == 0 && ch == '.';
        let leading_space = i == 0 && ch == ' ';
        let trailing_dot_or_space = i + 1 == char_count && (ch == '.' || ch == ' ');
        if leading_dot || leading_space || trailing_dot_or_space || needs_escape(ch) {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        } else {
            out.push(ch);
        }
    }
    // If the result collides with a Windows reserved device name, escape the
    // first character so the name becomes distinct without changing its
    // human-readable shape beyond the percent-encoded prefix.
    if is_windows_reserved_stem(&out) {
        let mut first = out.chars();
        if let Some(ch) = first.next() {
            let rest: String = first.collect();
            let mut buf = [0u8; 4];
            let mut escaped = String::with_capacity(out.len() + 2);
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                escaped.push_str(&format!("%{:02X}", b));
            }
            escaped.push_str(&rest);
            out = escaped;
        }
    }
    // `<Name> [N]` is Ro Sync's generated sibling-disambiguation grammar.
    // Escape the opening bracket when it is part of the literal Roblox name
    // so reverse mapping cannot silently strip it as an allocator ordinal.
    if parse_disambiguated(&out).is_some() {
        if let Some(open) = out.rfind(" [") {
            let ordinal = out[open + 2..out.len() - 1].to_string();
            out.replace_range(open.., &format!(" %5B{ordinal}%5D"));
        }
    }
    out
}

/// Reverse of [`encode_name`]. Best-effort: invalid escapes are passed through.
pub fn decode_name(encoded: &str) -> String {
    if encoded == EMPTY_NAME_SENTINEL {
        return String::new();
    }

    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Validate an already-encoded physical path component against the common
/// Windows/macOS/Linux representation budget.
///
/// Ro Sync's canonical fragments are ASCII-only, so byte length is also the
/// Windows UTF-16 code-unit length. A lossless shortening scheme would require
/// persisted identity metadata; until one exists, callers must reject an
/// overlong fragment before applying any part of a bootstrap transaction.
pub fn validate_portable_component(fragment: &str) -> Result<(), String> {
    if fragment.is_empty() {
        return Err("generated filesystem component is empty".to_string());
    }
    if fragment.len() > MAX_PORTABLE_COMPONENT_BYTES {
        return Err(format!(
            "generated filesystem component is {} bytes; portable limit is {}",
            fragment.len(),
            MAX_PORTABLE_COMPONENT_BYTES
        ));
    }
    Ok(())
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Instance → path
// ---------------------------------------------------------------------------

/// What the caller knows about an instance when asking for its filename.
#[derive(Debug, Clone)]
pub struct InstanceDescriptor<'a> {
    pub class: &'a str,
    pub name: &'a str,
    pub has_children: bool,
}

/// The placement decided for an instance on disk, relative to its parent dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFragment {
    pub fragment: String,
    pub is_dir: bool,
}

/// Decide the on-disk fragment for an instance, avoiding any names already in `taken`.
///
/// Sibling collisions use numeric `[N]` suffixes (1-based). The first sibling
/// with a given name keeps the bare fragment; subsequent siblings get
/// `Name [1]`, `Name [2]`, … applied *before* the script extension (so a
/// `ModuleScript` named `Thing` that collides becomes `Thing [1].luau`).
pub fn instance_to_path(inst: &InstanceDescriptor, taken: &[String]) -> PathFragment {
    let mut allocator = PathFragmentAllocator::from_taken(taken.iter().map(String::as_str));
    allocator.allocate(inst)
}

/// Incremental sibling-fragment allocator for wide directories.
///
/// `instance_to_path` accepts a slice for convenient one-off calls, but
/// repeatedly scanning that slice makes an N-child bootstrap quadratic.
/// This allocator preserves the same case-insensitive collision and ordinal
/// rules while indexing reserved fragments once.
#[derive(Debug, Default)]
pub struct PathFragmentAllocator {
    taken: HashSet<String>,
    next_ordinal: HashMap<String, usize>,
}

impl PathFragmentAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_taken<'a>(taken: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            taken: taken
                .into_iter()
                .map(|fragment| fragment.to_ascii_lowercase())
                .collect(),
            next_ordinal: HashMap::new(),
        }
    }

    pub fn allocate(&mut self, inst: &InstanceDescriptor<'_>) -> PathFragment {
        let encoded = encode_name(inst.name);
        let script = ScriptClass::from_class(inst.class);
        let is_dir = script.is_none() || inst.has_children;
        let extension = match script {
            Some(sc) if !inst.has_children => sc.suffix(),
            _ => "",
        };
        let make_fragment = |stem: &str| format!("{stem}{extension}");

        let base = make_fragment(&encoded);
        let base_key = base.to_ascii_lowercase();
        if self.taken.insert(base_key.clone()) {
            self.next_ordinal.entry(base_key).or_insert(1);
            return PathFragment {
                fragment: base,
                is_dir,
            };
        }

        let mut ordinal = self.next_ordinal.get(&base_key).copied().unwrap_or(1);
        loop {
            let stem = format!("{encoded} [{ordinal}]");
            let candidate = make_fragment(&stem);
            ordinal = ordinal
                .checked_add(1)
                .expect("filesystem sibling ordinal exceeds addressable memory");
            if self.taken.insert(candidate.to_ascii_lowercase()) {
                self.next_ordinal.insert(base_key, ordinal);
                return PathFragment {
                    fragment: candidate,
                    is_dir,
                };
            }
        }
    }
}

fn fragments_equal(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ---------------------------------------------------------------------------
// Path → instance
// ---------------------------------------------------------------------------

/// Instance data extracted from a single filesystem node. Does not recurse into
/// children — callers iterate directories themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInstance {
    pub name: String,
    pub class: String,
    pub is_dir: bool,
    /// The path is a directory whose identity is defined by an `init (<Name>).*.luau`
    /// child (i.e. a script-with-children).
    pub is_script_with_children: bool,
    pub script_class: Option<ScriptClass>,
}

/// Resolve a filesystem path to instance metadata, or `Ok(None)` when the path
/// should be skipped (e.g. `.meta.json`, plain `init (..).luau` files that only
/// describe their parent). Directory class resolution is init-script-wins-else-Folder.
pub fn path_to_instance_meta(path: &Path) -> io::Result<Option<PathInstance>> {
    let file_name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return Ok(None),
    };
    if file_name == META_FILE {
        return Ok(None);
    }

    let Some(meta) = metadata_no_follow(path)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("filesystem path does not exist: {}", path.display()),
        ));
    };

    if meta.is_file() {
        if parse_init_file(file_name).is_some() {
            return Ok(None);
        }
        let Some((script, stem)) = classify_script_file(file_name) else {
            return Ok(None);
        };
        let raw_name = match parse_disambiguated(&stem) {
            Some((n, _)) => n,
            None => stem,
        };
        return Ok(Some(PathInstance {
            name: decode_name(&raw_name),
            class: script.class_name().to_string(),
            is_dir: false,
            is_script_with_children: false,
            script_class: Some(script),
        }));
    }

    if !meta.is_dir() {
        return Ok(None);
    }

    let dir_name_raw = file_name.to_string();
    let dir_display = match parse_disambiguated(&dir_name_raw) {
        Some((n, _)) => n,
        None => dir_name_raw,
    };
    let decoded_dir_name = decode_name(&dir_display);

    let index = PortableDirectoryIndex::read(path)?;
    let init_entry = index.unique_init_source().and_then(|entry| {
        parse_init_file(&entry.fragment).or_else(|| {
            parse_plain_init_file(&entry.fragment).map(|sc| (sc, decoded_dir_name.clone()))
        })
    });
    let (name, class, script_class) = if let Some((sc, inner)) = init_entry.clone() {
        (inner, sc.class_name().to_string(), Some(sc))
    } else {
        (decoded_dir_name, "Folder".to_string(), None)
    };

    Ok(Some(PathInstance {
        name,
        class,
        is_dir: true,
        is_script_with_children: init_entry.is_some(),
        script_class,
    }))
}

pub fn is_empty_plain_folder(path: &Path) -> io::Result<bool> {
    let Some(inst) = path_to_instance_meta(path)? else {
        return Ok(false);
    };
    if inst.class != "Folder" || !inst.is_dir || inst.is_script_with_children {
        return Ok(false);
    }
    for entry in PortableDirectoryIndex::read(path)?.entries() {
        if entry.fragment == META_FILE || entry.fragment == ".DS_Store" {
            continue;
        }
        if entry.kind != SafeEntryKind::File && entry.kind != SafeEntryKind::Directory {
            return Ok(false);
        }
        return Ok(false);
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(tempfile::TempDir);
    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(
                tempfile::Builder::new()
                    .prefix(&format!("rosync-fsmap-{tag}-"))
                    .tempdir()
                    .unwrap(),
            )
        }
        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    // ----- classify_script_file / parse_init_file / parse_disambiguated -----

    #[test]
    fn classify_plain_luau() {
        let (c, s) = classify_script_file("Config.luau").unwrap();
        assert_eq!(c, ScriptClass::ModuleScript);
        assert_eq!(s, "Config");
    }

    #[test]
    fn classify_plain_lua() {
        let (c, s) = classify_script_file("React.lua").unwrap();
        assert_eq!(c, ScriptClass::ModuleScript);
        assert_eq!(s, "React");
    }

    #[test]
    fn classify_server_luau() {
        let (c, s) = classify_script_file("Main.server.luau").unwrap();
        assert_eq!(c, ScriptClass::Script);
        assert_eq!(s, "Main");
    }

    #[test]
    fn classify_client_luau() {
        let (c, s) = classify_script_file("UI.client.luau").unwrap();
        assert_eq!(c, ScriptClass::LocalScript);
        assert_eq!(s, "UI");
    }

    #[test]
    fn classify_lua_script_variants() {
        let (c, s) = classify_script_file("Main.server.lua").unwrap();
        assert_eq!(c, ScriptClass::Script);
        assert_eq!(s, "Main");

        let (c, s) = classify_script_file("UI.client.lua").unwrap();
        assert_eq!(c, ScriptClass::LocalScript);
        assert_eq!(s, "UI");
    }

    #[test]
    fn classify_rejects_non_script_files() {
        assert!(classify_script_file("README.md").is_none());
        assert!(classify_script_file("wally.toml").is_none());
    }

    #[test]
    fn parse_init_variants() {
        assert_eq!(
            parse_init_file("init (Thing).luau"),
            Some((ScriptClass::ModuleScript, "Thing".to_string()))
        );
        assert_eq!(
            parse_init_file("init (Net).server.luau"),
            Some((ScriptClass::Script, "Net".to_string()))
        );
        assert_eq!(
            parse_init_file("init (UI).client.luau"),
            Some((ScriptClass::LocalScript, "UI".to_string()))
        );
        assert_eq!(
            parse_init_file("init (Package).lua"),
            Some((ScriptClass::ModuleScript, "Package".to_string()))
        );
        assert_eq!(
            parse_init_file("init (ServerPackage).server.lua"),
            Some((ScriptClass::Script, "ServerPackage".to_string()))
        );
        assert_eq!(parse_init_file("foo.luau"), None);
        assert_eq!(parse_init_file("init ().luau"), None);
        assert_eq!(
            parse_plain_init_file("init.lua"),
            Some(ScriptClass::ModuleScript)
        );
        assert_eq!(
            parse_plain_init_file("init.luau"),
            Some(ScriptClass::ModuleScript)
        );
        assert_eq!(
            parse_plain_init_file("init.server.lua"),
            Some(ScriptClass::Script)
        );
        assert!(is_init_file("init.lua"));
    }

    #[test]
    fn parse_disambig_variants() {
        assert_eq!(
            parse_disambiguated("Thing [1]"),
            Some(("Thing".to_string(), 1))
        );
        assert_eq!(
            parse_disambiguated("Thing [12]"),
            Some(("Thing".to_string(), 12))
        );
        assert_eq!(
            parse_disambiguated("Thing [1] [2]"),
            Some(("Thing [1]".to_string(), 2))
        );
        assert_eq!(parse_disambiguated("Thing"), None);
        assert_eq!(parse_disambiguated("Thing [foo]"), None);
        assert_eq!(parse_disambiguated("Thing []"), None);
        // Old `(ClassName)` form should NOT be recognized as disambiguation.
        assert_eq!(parse_disambiguated("Thing (Folder)"), None);
    }

    #[test]
    fn parse_init_file_strips_ordinal() {
        assert_eq!(
            parse_init_file("init (Thing [1]).luau"),
            Some((ScriptClass::ModuleScript, "Thing".to_string()))
        );
        assert_eq!(
            parse_init_file("init (Thing).server.luau"),
            Some((ScriptClass::Script, "Thing".to_string()))
        );
        assert_eq!(
            parse_init_file("init (a%2Fb).luau"),
            Some((ScriptClass::ModuleScript, "a/b".to_string()))
        );
        assert_eq!(
            parse_init_file("init (%C3%89).luau"),
            Some((ScriptClass::ModuleScript, "É".to_string()))
        );
    }

    // ----- encode / decode ----------------------------------------------

    #[test]
    fn encode_roundtrip_safe_chars() {
        let name = "Hello World";
        assert_eq!(encode_name(name), "Hello World");
        assert_eq!(decode_name(&encode_name(name)), name);
    }

    #[test]
    fn encode_escapes_slash_and_leading_dot() {
        assert_eq!(encode_name("a/b"), "a%2Fb");
        assert_eq!(encode_name(".secret"), "%2Esecret");
        assert_eq!(decode_name("a%2Fb"), "a/b");
        assert_eq!(decode_name("%2Esecret"), ".secret");
    }

    #[test]
    fn encode_escapes_windows_illegal_chars() {
        // <>:"|?* all forbidden on Windows — must round-trip via percent-encoding.
        let inputs = ["a<b", "a>b", "a:b", "a\"b", "a|b", "a?b", "a*b"];
        for s in inputs {
            let enc = encode_name(s);
            assert!(
                !enc.contains(['<', '>', '"', '|', '?', '*']),
                "encoded form of {s:?} still contains a Windows-illegal char: {enc}"
            );
            assert_eq!(decode_name(&enc), s);
        }
    }

    #[test]
    fn encode_escapes_trailing_dot_and_space() {
        assert!(encode_name("Foo.").ends_with("%2E"));
        assert!(encode_name("Foo ").ends_with("%20"));
        assert_eq!(decode_name(&encode_name("Foo.")), "Foo.");
        assert_eq!(decode_name(&encode_name("Foo ")), "Foo ");
    }

    #[test]
    fn encode_escapes_leading_ascii_space_for_win32_portability() {
        assert_eq!(encode_name(" Foo"), "%20Foo");
        assert_eq!(encode_name("  Foo"), "%20 Foo");
        assert_eq!(decode_name(&encode_name(" Foo")), " Foo");
    }

    #[test]
    fn empty_name_uses_a_non_collapsing_reversible_sentinel() {
        assert_eq!(encode_name(""), EMPTY_NAME_SENTINEL);
        assert_eq!(decode_name(EMPTY_NAME_SENTINEL), "");

        // A literal name equal to the sentinel remains distinct.
        assert_eq!(encode_name(EMPTY_NAME_SENTINEL), "%25_");
        assert_eq!(
            decode_name(&encode_name(EMPTY_NAME_SENTINEL)),
            EMPTY_NAME_SENTINEL
        );
    }

    #[test]
    fn encode_escapes_windows_reserved_names() {
        // Exact reserved name — first char must be escaped so the result isn't CON.
        let enc = encode_name("CON");
        assert_ne!(enc.to_ascii_uppercase(), "CON");
        assert_eq!(decode_name(&enc), "CON");

        // Reserved + extension is still reserved on Windows.
        let enc2 = encode_name("PRN");
        assert_ne!(enc2.to_ascii_uppercase(), "PRN");

        // Case-insensitive.
        assert_ne!(encode_name("nul").to_ascii_uppercase(), "NUL");

        // CreateFile also recognizes the console device aliases even though
        // the shorter Win32 naming-rule summary lists only CON.
        assert_eq!(encode_name("CONIN$"), "%43ONIN$");
        assert_eq!(encode_name("conout$.txt"), "%63onout$.txt");
        assert_eq!(decode_name(&encode_name("CONOUT$")), "CONOUT$");

        // Microsoft also reserves COM/LPT with superscript 1, 2, and 3.
        // Encoding all non-ASCII UTF-8 bytes makes those spellings portable
        // without growing a second device-name table in either codec.
        let superscript = encode_name("COM¹");
        assert_eq!(superscript, "COM%C2%B9");
        assert_eq!(decode_name(&superscript), "COM¹");
        assert!(!is_windows_reserved_stem(&superscript));

        // Non-reserved stems are left alone.
        assert_eq!(encode_name("CONFIG"), "CONFIG");
        assert_eq!(encode_name("COM"), "COM"); // only COM1..COM9 are reserved
    }

    #[test]
    fn reserved_name_with_literal_ordinal_suffix_applies_both_escapes() {
        let encoded = encode_name("CON.foo [1]");
        assert_eq!(encoded, "%43ON.foo %5B1%5D");
        assert_eq!(decode_name(&encoded), "CON.foo [1]");
        assert_eq!(parse_disambiguated(&encoded), None);
        assert!(!is_windows_reserved_stem(&encoded));

        let lowercase = encode_name("nul.data [12]");
        assert_eq!(lowercase, "%6Eul.data %5B12%5D");
        assert_eq!(decode_name(&lowercase), "nul.data [12]");
        assert_eq!(parse_disambiguated(&lowercase), None);
        assert!(!is_windows_reserved_stem(&lowercase));
    }

    #[test]
    fn encode_roundtrip_unicode() {
        let name = "配置/αβ.data";
        let enc = encode_name(name);
        assert!(!enc.contains('/'));
        assert!(enc.is_ascii());
        assert_eq!(decode_name(&enc), name);
    }

    #[test]
    fn unicode_names_have_distinct_portable_ascii_fragments() {
        let names = ["É", "é", "e\u{301}"];
        let encoded: Vec<String> = names.iter().map(|name| encode_name(name)).collect();
        assert!(encoded.iter().all(|name| name.is_ascii()));
        assert_eq!(encoded, ["%C3%89", "%C3%A9", "e%CC%81"]);
        assert_eq!(
            encoded
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            names.len()
        );
        for (encoded, original) in encoded.iter().zip(names) {
            assert_eq!(decode_name(encoded), original);
        }
    }

    #[test]
    fn encode_escapes_percent_marker() {
        assert_eq!(encode_name("Value%2FName"), "Value%252FName");
        assert_eq!(decode_name(&encode_name("Value%2FName")), "Value%2FName");
        assert_eq!(decode_name(&encode_name("%")), "%");
    }

    #[test]
    fn literal_disambiguation_suffix_roundtrips_without_becoming_an_ordinal() {
        let encoded = encode_name("Thing [1]");
        assert_eq!(encoded, "Thing %5B1%5D");
        assert_eq!(decode_name(&encoded), "Thing [1]");
        assert_eq!(parse_disambiguated(&encoded), None);

        let fragment = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Thing [1]",
                has_children: false,
            },
            &[],
        );
        assert_eq!(fragment.fragment, "Thing %5B1%5D.luau");
    }

    #[test]
    fn empty_sibling_names_keep_distinct_leaf_and_directory_fragments() {
        let mut folders = PathFragmentAllocator::new();
        let first_folder = folders.allocate(&InstanceDescriptor {
            class: "Folder",
            name: "",
            has_children: true,
        });
        let second_folder = folders.allocate(&InstanceDescriptor {
            class: "Folder",
            name: "",
            has_children: true,
        });
        assert_eq!(first_folder.fragment, "%_");
        assert_eq!(second_folder.fragment, "%_ [1]");
        let (folder_name, folder_ordinal) = parse_disambiguated(&second_folder.fragment).unwrap();
        assert_eq!(decode_name(&folder_name), "");
        assert_eq!(folder_ordinal, 1);

        let mut scripts = PathFragmentAllocator::new();
        let first_script = scripts.allocate(&InstanceDescriptor {
            class: "ModuleScript",
            name: "",
            has_children: false,
        });
        let second_script = scripts.allocate(&InstanceDescriptor {
            class: "ModuleScript",
            name: "",
            has_children: false,
        });
        assert_eq!(first_script.fragment, "%_.luau");
        assert_eq!(second_script.fragment, "%_ [1].luau");
        let temp = TempDir::new("empty-script-name");
        let empty_script = temp.path().join(&first_script.fragment);
        fs::write(&empty_script, "return true").unwrap();
        assert_eq!(
            path_to_instance_meta(&empty_script).unwrap().unwrap().name,
            ""
        );

        let empty_script_dir = temp.path().join(EMPTY_NAME_SENTINEL);
        fs::create_dir(&empty_script_dir).unwrap();
        let empty_init = portable_init_file_name("", ScriptClass::Script);
        assert_eq!(empty_init, "init (%_).server.luau");
        fs::write(empty_script_dir.join(empty_init), "print('ok')").unwrap();
        let metadata = path_to_instance_meta(&empty_script_dir).unwrap().unwrap();
        assert_eq!(metadata.name, "");
        assert_eq!(metadata.class, "Script");
        assert!(metadata.is_script_with_children);
    }

    // ----- instance_to_path --------------------------------------------

    #[test]
    fn instance_to_path_module_script() {
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Config",
                has_children: false,
            },
            &[],
        );
        assert_eq!(f.fragment, "Config.luau");
        assert!(!f.is_dir);
    }

    #[test]
    fn instance_to_path_server_script() {
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "Script",
                name: "Main",
                has_children: false,
            },
            &[],
        );
        assert_eq!(f.fragment, "Main.server.luau");
    }

    #[test]
    fn instance_to_path_script_with_children_is_dir() {
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Net",
                has_children: true,
            },
            &[],
        );
        assert_eq!(f.fragment, "Net");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_folder() {
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "Folder",
                name: "Shared",
                has_children: true,
            },
            &[],
        );
        assert_eq!(f.fragment, "Shared");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_collision_numbered_suffix() {
        // A ModuleScript `Thing.luau` already exists — a sibling Folder `Thing`
        // uses a different fragment (`Thing/`) so doesn't collide.
        let taken = vec!["Thing.luau".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "Folder",
                name: "Thing",
                has_children: false,
            },
            &taken,
        );
        assert_eq!(f.fragment, "Thing");
        assert!(f.is_dir);

        // Two ModuleScripts named Thing → second gets `[1]` suffix.
        let f2 = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Thing",
                has_children: false,
            },
            &taken,
        );
        assert_eq!(f2.fragment, "Thing [1].luau");
    }

    #[test]
    fn instance_to_path_collision_escalates_ordinal() {
        let taken = vec!["Thing.luau".to_string(), "Thing [1].luau".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Thing",
                has_children: false,
            },
            &taken,
        );
        assert_eq!(f.fragment, "Thing [2].luau");
    }

    #[test]
    fn instance_to_path_collision_folders_numbered() {
        let taken = vec!["Shared".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "Folder",
                name: "Shared",
                has_children: true,
            },
            &taken,
        );
        assert_eq!(f.fragment, "Shared [1]");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_avoids_case_only_collisions() {
        let taken = vec!["Config.luau".to_string(), "Shared".to_string()];
        let script = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "config",
                has_children: false,
            },
            &taken,
        );
        assert_eq!(script.fragment, "config [1].luau");

        let folder = instance_to_path(
            &InstanceDescriptor {
                class: "Folder",
                name: "shared",
                has_children: true,
            },
            &taken,
        );
        assert_eq!(folder.fragment, "shared [1]");
    }

    #[test]
    fn duplicate_matching_uses_ascii_case_rules_on_portable_fragments() {
        assert!(fragments_equal("Config", "config"));
        assert!(!fragments_equal(&encode_name("É"), &encode_name("é")));
    }

    #[test]
    fn instance_to_path_collision_script_with_children_numbered() {
        let taken = vec!["Net".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Net",
                has_children: true,
            },
            &taken,
        );
        assert_eq!(f.fragment, "Net [1]");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_encodes_special_chars() {
        let f = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "a/b",
                has_children: false,
            },
            &[],
        );
        assert_eq!(f.fragment, "a%2Fb.luau");
    }

    #[test]
    fn fragment_allocator_preserves_case_insensitive_collision_rules() {
        let mut allocator = PathFragmentAllocator::from_taken(["Config.luau", "Thing [1].luau"]);
        let config = allocator.allocate(&InstanceDescriptor {
            class: "ModuleScript",
            name: "config",
            has_children: false,
        });
        let thing = allocator.allocate(&InstanceDescriptor {
            class: "ModuleScript",
            name: "Thing",
            has_children: false,
        });
        let duplicate = allocator.allocate(&InstanceDescriptor {
            class: "ModuleScript",
            name: "Thing",
            has_children: false,
        });

        assert_eq!(config.fragment, "config [1].luau");
        assert_eq!(thing.fragment, "Thing.luau");
        assert_eq!(duplicate.fragment, "Thing [2].luau");
    }

    #[test]
    fn fragment_allocator_handles_tens_of_thousands_of_wide_siblings() {
        const WIDTH: usize = 20_000;
        let mut allocator = PathFragmentAllocator::new();
        for index in 0..WIDTH {
            let name = format!("Item{index:05}");
            let fragment = allocator.allocate(&InstanceDescriptor {
                class: "ModuleScript",
                name: &name,
                has_children: false,
            });
            assert_eq!(fragment.fragment, format!("{name}.luau"));
        }
        assert_eq!(allocator.taken.len(), WIDTH);
    }

    #[test]
    fn fragment_allocator_keeps_five_digit_duplicate_ordinals_roundtrippable() {
        let module = InstanceDescriptor {
            class: "ModuleScript",
            name: "Repeated",
            has_children: false,
        };
        let mut module_allocator = PathFragmentAllocator::new();
        let mut module_fragment = None;
        for _ in 0..=10_000 {
            module_fragment = Some(module_allocator.allocate(&module));
        }
        let module_fragment = module_fragment.unwrap();
        assert_eq!(module_fragment.fragment, "Repeated [10000].luau");
        let (class, stem) = classify_script_file(&module_fragment.fragment).unwrap();
        assert_eq!(class, ScriptClass::ModuleScript);
        assert_eq!(
            parse_disambiguated(&stem),
            Some(("Repeated".to_string(), 10_000))
        );

        let folder = InstanceDescriptor {
            class: "Folder",
            name: "Repeated",
            has_children: true,
        };
        let mut folder_allocator = PathFragmentAllocator::new();
        let mut folder_fragment = None;
        for _ in 0..=10_000 {
            folder_fragment = Some(folder_allocator.allocate(&folder));
        }
        let folder_fragment = folder_fragment.unwrap();
        assert_eq!(folder_fragment.fragment, "Repeated [10000]");
        assert_eq!(
            parse_disambiguated(&folder_fragment.fragment),
            Some(("Repeated".to_string(), 10_000))
        );
    }

    #[test]
    fn one_off_path_mapping_keeps_five_digit_duplicate_ordinal_before_extension() {
        let mut taken = Vec::with_capacity(10_000);
        taken.push("Repeated.luau".to_string());
        taken.extend((1..10_000).map(|ordinal| format!("Repeated [{ordinal}].luau")));

        let fragment = instance_to_path(
            &InstanceDescriptor {
                class: "ModuleScript",
                name: "Repeated",
                has_children: false,
            },
            &taken,
        );
        assert_eq!(fragment.fragment, "Repeated [10000].luau");
    }

    #[test]
    fn overlong_duplicate_component_is_lossless_but_fails_portable_preflight() {
        let name = "A".repeat(MAX_PORTABLE_COMPONENT_BYTES - ".luau".len());
        let descriptor = InstanceDescriptor {
            class: "ModuleScript",
            name: &name,
            has_children: false,
        };
        let mut allocator = PathFragmentAllocator::new();
        let bare = allocator.allocate(&descriptor);
        let duplicate = allocator.allocate(&descriptor);

        assert_eq!(bare.fragment.len(), MAX_PORTABLE_COMPONENT_BYTES);
        assert!(validate_portable_component(&bare.fragment).is_ok());
        assert!(duplicate.fragment.ends_with(" [1].luau"));
        assert!(duplicate.fragment.len() > MAX_PORTABLE_COMPONENT_BYTES);
        assert!(validate_portable_component(&duplicate.fragment).is_err());
        let (class, stem) = classify_script_file(&duplicate.fragment).unwrap();
        assert_eq!(class, ScriptClass::ModuleScript);
        let (decoded_name, ordinal) = parse_disambiguated(&stem).unwrap();
        assert_eq!(ordinal, 1);
        assert_eq!(decoded_name, name);
    }

    #[test]
    fn percent_expanded_unicode_component_fails_portable_preflight() {
        let encoded = encode_name(&"😀".repeat(100));
        assert_eq!(decode_name(&encoded), "😀".repeat(100));
        assert!(encoded.len() > MAX_PORTABLE_COMPONENT_BYTES);
        let error = validate_portable_component(&encoded).unwrap_err();
        assert!(error.contains("portable limit is 255"));
    }

    #[test]
    fn long_script_directory_uses_a_portable_plain_init_marker() {
        let name = "A".repeat(MAX_PORTABLE_COMPONENT_BYTES - 1);
        let init = portable_init_file_name(&name, ScriptClass::LocalScript);
        assert_eq!(init, "init.client.luau");
        assert!(init.len() <= MAX_PORTABLE_COMPONENT_BYTES);

        let ordinary = portable_init_file_name("Controller", ScriptClass::ModuleScript);
        assert_eq!(ordinary, "init (Controller).luau");
    }

    // ----- path_to_instance_meta ---------------------------------------

    #[test]
    fn path_to_instance_module_script() {
        let d = TempDir::new("mod");
        let p = d.path().join("Config.luau");
        fs::write(&p, b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Config");
        assert_eq!(inst.class, "ModuleScript");
        assert!(!inst.is_dir);
    }

    #[test]
    fn path_to_instance_wally_lua_module_script() {
        let d = TempDir::new("wally-lua");
        let p = d.path().join("React.lua");
        fs::write(&p, b"return require(script.Parent._Index.react)").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "React");
        assert_eq!(inst.class, "ModuleScript");
        assert!(!inst.is_dir);
    }

    #[test]
    fn path_to_instance_folder_default() {
        let d = TempDir::new("folder");
        let p = d.path().join("Shared");
        fs::create_dir(&p).unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Shared");
        assert_eq!(inst.class, "Folder");
        assert!(inst.is_dir);
        assert!(!inst.is_script_with_children);
        assert!(is_empty_plain_folder(&p).unwrap());
        fs::write(p.join("Child.luau"), b"return {}").unwrap();
        assert!(!is_empty_plain_folder(&p).unwrap());
    }

    #[test]
    fn path_to_instance_script_with_children() {
        let d = TempDir::new("swc");
        let p = d.path().join("Net");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("init (Net).luau"), b"return {}").unwrap();
        fs::write(p.join("Helper.luau"), b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Net");
        assert_eq!(inst.class, "ModuleScript");
        assert!(inst.is_script_with_children);
        assert_eq!(inst.script_class, Some(ScriptClass::ModuleScript));
    }

    #[test]
    fn path_to_instance_decodes_portable_script_with_children_names() {
        for name in ["a/b", "É", "é", "e\u{301}"] {
            let d = TempDir::new("portable-swc");
            let encoded = encode_name(name);
            let p = d.path().join(&encoded);
            fs::create_dir(&p).unwrap();
            fs::write(p.join(format!("init ({encoded}).luau")), b"return {}").unwrap();
            let inst = path_to_instance_meta(&p).unwrap().unwrap();
            assert_eq!(inst.name, name);
            assert_eq!(inst.class, "ModuleScript");
            assert!(inst.is_script_with_children);
        }
    }

    #[test]
    fn path_to_instance_still_reads_legacy_literal_unicode_init() {
        let d = TempDir::new("legacy-unicode-swc");
        let p = d.path().join("É");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("init (É).luau"), b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "É");
        assert_eq!(inst.class, "ModuleScript");
    }

    #[test]
    fn path_to_instance_wally_plain_init_script_with_children() {
        let d = TempDir::new("wally-swc");
        let p = d.path().join("net");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("init.lua"), b"return {}").unwrap();
        fs::write(p.join("client.lua"), b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "net");
        assert_eq!(inst.class, "ModuleScript");
        assert!(inst.is_script_with_children);
        assert_eq!(inst.script_class, Some(ScriptClass::ModuleScript));
    }

    #[test]
    fn path_to_instance_script_with_children_server_variant() {
        let d = TempDir::new("swc-server");
        let p = d.path().join("Main");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("init (Main).server.luau"), b"-- svr").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.class, "Script");
        assert_eq!(inst.script_class, Some(ScriptClass::Script));
    }

    #[test]
    fn path_to_instance_disambiguated_file() {
        let d = TempDir::new("disambig");
        let p = d.path().join("Thing [1].luau");
        fs::write(&p, b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Thing");
        assert_eq!(inst.class, "ModuleScript");
    }

    #[test]
    fn path_to_instance_disambiguated_folder() {
        let d = TempDir::new("disambig-folder");
        let p = d.path().join("Shared [2]");
        fs::create_dir(&p).unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Shared");
        assert_eq!(inst.class, "Folder");
        assert!(inst.is_dir);
    }

    #[test]
    fn path_to_instance_disambiguated_script_with_children() {
        let d = TempDir::new("disambig-swc");
        let p = d.path().join("Net [1]");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("init (Net).luau"), b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        // Instance name comes from the init file's inner — not from the dir
        // fragment — so it's the clean `Net`, matching the Roblox instance.
        assert_eq!(inst.name, "Net");
        assert_eq!(inst.class, "ModuleScript");
        assert!(inst.is_script_with_children);
    }

    #[test]
    fn path_to_instance_skips_meta_and_init_files() {
        let d = TempDir::new("skip");
        let p = d.path();
        let meta_path = p.join(META_FILE);
        fs::write(&meta_path, "{}").unwrap();
        assert!(path_to_instance_meta(&meta_path).unwrap().is_none());

        let init_path = p.join("init (X).luau");
        fs::write(&init_path, "--").unwrap();
        assert!(path_to_instance_meta(&init_path).unwrap().is_none());
    }

    // ----- round-trip --------------------------------------------------

    #[test]
    fn roundtrip_module_script() {
        let d = TempDir::new("rt-mod");
        let desc = InstanceDescriptor {
            class: "ModuleScript",
            name: "Config",
            has_children: false,
        };
        let frag = instance_to_path(&desc, &[]);
        let p = d.path().join(&frag.fragment);
        fs::write(&p, b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, desc.name);
        assert_eq!(inst.class, desc.class);
    }

    #[test]
    fn roundtrip_name_with_slash() {
        let d = TempDir::new("rt-slash");
        let desc = InstanceDescriptor {
            class: "ModuleScript",
            name: "a/b",
            has_children: false,
        };
        let frag = instance_to_path(&desc, &[]);
        let p = d.path().join(&frag.fragment);
        fs::write(&p, b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "a/b");
    }

    #[test]
    fn roundtrip_script_with_children() {
        let d = TempDir::new("rt-swc");
        let desc = InstanceDescriptor {
            class: "Script",
            name: "Main",
            has_children: true,
        };
        let frag = instance_to_path(&desc, &[]);
        let dir_path = d.path().join(&frag.fragment);
        fs::create_dir(&dir_path).unwrap();
        // The caller would separately place `init (Main).server.luau` inside.
        fs::write(dir_path.join("init (Main).server.luau"), b"-- svr").unwrap();
        let inst = path_to_instance_meta(&dir_path).unwrap().unwrap();
        assert_eq!(inst.name, "Main");
        assert_eq!(inst.class, "Script");
        assert!(inst.is_script_with_children);
    }
}
