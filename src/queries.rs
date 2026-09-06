pub mod api;
pub mod auth;
pub mod clips;
pub mod collectibles;
pub mod context;
pub mod custom_map_aux_specs;
pub mod custom_maps;
pub mod hats;
pub mod listed_maps;
pub mod minigames;
pub mod moderation;
pub mod players;
pub mod stats;
pub mod user_settings;

pub use api::*;
pub use auth::*;
pub use clips::*;
pub use context::*;
pub use custom_map_aux_specs::*;
pub use custom_maps::*;
pub use moderation::*;
pub use players::*;
pub use stats::*;
pub use user_settings::*;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogValidationError {
    #[error("explicit {catalog} slug is blank after canonicalization")]
    BlankSlug { catalog: &'static str },
    #[error("duplicate {catalog} slug after canonicalization: {slug}")]
    DuplicateSlug { catalog: &'static str, slug: String },
    #[error("duplicate hat record UID after canonicalization: {uid}")]
    DuplicateRecordUid { uid: String },
}
