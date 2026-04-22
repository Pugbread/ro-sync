#![allow(dead_code)] // public API consumed by http routes + watcher (wired by sibling modules).

//! Filesystem ↔ Roblox Instance mapping.
//!
//! Conventions (scope: scripts + folders only):
//!   * `Foo.luau`            → ModuleScript named `Foo`
//!   * `Foo.server.luau`     → Script          named `Foo`
//!   * `Foo.client.luau`     → LocalScript     named `Foo`
//!   * `Foo/` (dir)          → Folder          named `Foo`
//!   * `Foo/init (Foo).luau` → the folder IS a ModuleScript named `Foo` whose children
//!                             are the other entries in the folder. `.server.luau` /
//!                             `.client.luau` variants pick Script / LocalScript.
//!   * Sibling name collisions are broken with numeric suffixes.
//!   * Unsafe characters in instance names are percent-encoded.

use std::fs;
use std::io;
use std::path::Path;

pub const META_FILE: &str = ".meta.json";

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

/// Classify a file name as a script, returning `(class, stem_without_ext)`.
///
/// Order matters: `.server.luau` and `.client.luau` must be tried before the
/// plain `.luau` suffix.
pub fn classify_script_file(file_name: &str) -> Option<(ScriptClass, String)> {
    if let Some(stem) = file_name.strip_suffix(".server.luau") {
        Some((ScriptClass::Script, stem.to_string()))
    } else if let Some(stem) = file_name.strip_suffix(".client.luau") {
        Some((ScriptClass::LocalScript, stem.to_string()))
    } else if let Some(stem) = file_name.strip_suffix(".luau") {
        Some((ScriptClass::ModuleScript, stem.to_string()))
    } else {
        None
    }
}

/// Parse an `init (<Name>).{server,client,}.luau` file name. The returned name
/// is the instance's *clean* name — any `[N]` disambiguation suffix inside the
/// parentheses is stripped so the inner matches the Roblox instance name.
pub fn parse_init_file(file_name: &str) -> Option<(ScriptClass, String)> {
    let (class, stem) = classify_script_file(file_name)?;
    let inner = stem.strip_prefix("init (")?.strip_suffix(')')?;
    if inner.is_empty() {
        return None;
    }
    let name = match parse_disambiguated(inner) {
        Some((n, _)) => n,
        None => inner.to_string(),
    };
    Some((class, name))
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
    matches!(ch, '/' | '\\' | '\0' | ':') || (ch as u32) < 0x20
}

/// Percent-encode characters that can't (or shouldn't) appear in POSIX paths.
pub fn encode_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, ch) in name.chars().enumerate() {
        let leading_dot = i == 0 && ch == '.';
        if leading_dot || needs_escape(ch) {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Reverse of [`encode_name`]. Best-effort: invalid escapes are passed through.
pub fn decode_name(encoded: &str) -> String {
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
    let encoded = encode_name(inst.name);
    let script = ScriptClass::from_class(inst.class);

    let is_dir = match script {
        Some(_) if inst.has_children => true,
        Some(_) => false,
        None => true,
    };

    let make_fragment = |stem: &str| -> String {
        match script {
            Some(_) if inst.has_children => stem.to_string(),
            Some(sc) => format!("{}{}", stem, sc.suffix()),
            None => stem.to_string(),
        }
    };

    let base = make_fragment(&encoded);
    if !taken.iter().any(|t| t == &base) {
        return PathFragment { fragment: base, is_dir };
    }

    for n in 1..10_000 {
        let candidate = make_fragment(&format!("{} [{}]", encoded, n));
        if !taken.iter().any(|t| t == &candidate) {
            return PathFragment { fragment: candidate, is_dir };
        }
    }
    PathFragment { fragment: format!("{}__{}", base, taken.len()), is_dir }
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

    let meta = fs::symlink_metadata(path)?;

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

    let mut init_entry: Option<(ScriptClass, String)> = None;
    for entry in fs::read_dir(path)? {
        let e = entry?;
        if let Some(n) = e.file_name().to_str() {
            if let Some(parsed) = parse_init_file(n) {
                init_entry = Some(parsed);
                break;
            }
        }
    }

    let dir_name_raw = file_name.to_string();
    let dir_display = match parse_disambiguated(&dir_name_raw) {
        Some((n, _)) => n,
        None => dir_name_raw,
    };

    let (name, class, script_class) = if let Some((sc, inner)) = init_entry.clone() {
        (inner, sc.class_name().to_string(), Some(sc))
    } else {
        (decode_name(&dir_display), "Folder".to_string(), None)
    };

    Ok(Some(PathInstance {
        name,
        class,
        is_dir: true,
        is_script_with_children: init_entry.is_some(),
        script_class,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "rosync-fsmap-{}-{}-{}",
                tag,
                std::process::id(),
                rand_token()
            ));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rand_token() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:x}", nanos)
    }

    // ----- classify_script_file / parse_init_file / parse_disambiguated -----

    #[test]
    fn classify_plain_luau() {
        let (c, s) = classify_script_file("Config.luau").unwrap();
        assert_eq!(c, ScriptClass::ModuleScript);
        assert_eq!(s, "Config");
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
    fn classify_rejects_non_luau() {
        assert!(classify_script_file("README.md").is_none());
        assert!(classify_script_file("x.lua").is_none());
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
        assert_eq!(parse_init_file("foo.luau"), None);
        assert_eq!(parse_init_file("init ().luau"), None);
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
    fn encode_roundtrip_unicode() {
        let name = "配置/αβ.data";
        let enc = encode_name(name);
        assert!(!enc.contains('/'));
        assert_eq!(decode_name(&enc), name);
    }

    // ----- instance_to_path --------------------------------------------

    #[test]
    fn instance_to_path_module_script() {
        let f = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "Config", has_children: false },
            &[],
        );
        assert_eq!(f.fragment, "Config.luau");
        assert!(!f.is_dir);
    }

    #[test]
    fn instance_to_path_server_script() {
        let f = instance_to_path(
            &InstanceDescriptor { class: "Script", name: "Main", has_children: false },
            &[],
        );
        assert_eq!(f.fragment, "Main.server.luau");
    }

    #[test]
    fn instance_to_path_script_with_children_is_dir() {
        let f = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "Net", has_children: true },
            &[],
        );
        assert_eq!(f.fragment, "Net");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_folder() {
        let f = instance_to_path(
            &InstanceDescriptor { class: "Folder", name: "Shared", has_children: true },
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
            &InstanceDescriptor { class: "Folder", name: "Thing", has_children: false },
            &taken,
        );
        assert_eq!(f.fragment, "Thing");
        assert!(f.is_dir);

        // Two ModuleScripts named Thing → second gets `[1]` suffix.
        let f2 = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "Thing", has_children: false },
            &taken,
        );
        assert_eq!(f2.fragment, "Thing [1].luau");
    }

    #[test]
    fn instance_to_path_collision_escalates_ordinal() {
        let taken = vec![
            "Thing.luau".to_string(),
            "Thing [1].luau".to_string(),
        ];
        let f = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "Thing", has_children: false },
            &taken,
        );
        assert_eq!(f.fragment, "Thing [2].luau");
    }

    #[test]
    fn instance_to_path_collision_folders_numbered() {
        let taken = vec!["Shared".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor { class: "Folder", name: "Shared", has_children: true },
            &taken,
        );
        assert_eq!(f.fragment, "Shared [1]");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_collision_script_with_children_numbered() {
        let taken = vec!["Net".to_string()];
        let f = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "Net", has_children: true },
            &taken,
        );
        assert_eq!(f.fragment, "Net [1]");
        assert!(f.is_dir);
    }

    #[test]
    fn instance_to_path_encodes_special_chars() {
        let f = instance_to_path(
            &InstanceDescriptor { class: "ModuleScript", name: "a/b", has_children: false },
            &[],
        );
        assert_eq!(f.fragment, "a%2Fb.luau");
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
    fn path_to_instance_folder_default() {
        let d = TempDir::new("folder");
        let p = d.path().join("Shared");
        fs::create_dir(&p).unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "Shared");
        assert_eq!(inst.class, "Folder");
        assert!(inst.is_dir);
        assert!(!inst.is_script_with_children);
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
        let desc = InstanceDescriptor { class: "ModuleScript", name: "Config", has_children: false };
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
        let desc = InstanceDescriptor { class: "ModuleScript", name: "a/b", has_children: false };
        let frag = instance_to_path(&desc, &[]);
        let p = d.path().join(&frag.fragment);
        fs::write(&p, b"return {}").unwrap();
        let inst = path_to_instance_meta(&p).unwrap().unwrap();
        assert_eq!(inst.name, "a/b");
    }

    #[test]
    fn roundtrip_script_with_children() {
        let d = TempDir::new("rt-swc");
        let desc = InstanceDescriptor { class: "Script", name: "Main", has_children: true };
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
