//! Destinations that own the whole body. The document views (editor, image
//! viewer) keep the sidebar and breadcrumb; Settings and Welcome replace them.

pub(crate) mod editor;
pub(crate) mod settings;
pub(crate) mod welcome;

pub(crate) use editor::*;
pub(crate) use settings::*;
pub(crate) use welcome::*;
