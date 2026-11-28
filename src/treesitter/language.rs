//! The source language a file is written in, shared by the Stage 2 summary
//! and the frontend that produced it.

/// A source language a frontend reads. Rust is the only one this backend
/// knows; the enum keeps the shape a second frontend slots into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
}

impl Language {
    /// Whether values of the language are references to objects, so that
    /// passing one on hands out the object itself. Rust's values are not.
    pub fn objects_passed_by_ref(&self) -> bool {
        match self {
            Language::Rust => false,
        }
    }
}
