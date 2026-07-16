//! Canonical filesystem-sync scope shared by daemon subsystems.
//!
//! Keep the plugin's generated `ALLOWED_CLASSES` table in protocol lockstep
//! with this list. Rust code must import this module instead of restating it.

pub const CLASSES: &[&str] = &["Folder", "Script", "LocalScript", "ModuleScript"];

pub fn contains(class: &str) -> bool {
    CLASSES.contains(&class)
}
