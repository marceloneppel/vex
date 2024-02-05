use std::fs;

use anyhow::Context;
use log::{log_enabled, trace};
use tree_sitter::{Parser, Tree};

use crate::{scriptlets::PrettyPath, supported_language::SupportedLanguage};

pub struct SourceFile {
    pub path: PrettyPath,
    pub content: String,
    pub tree: Tree,
    pub lang: SupportedLanguage,
}

impl SourceFile {
    pub fn load_if_supported(path: PrettyPath) -> Option<anyhow::Result<Self>> {
        let Some(extension) = path.extension() else {
            if log_enabled!(log::Level::Trace) {
                trace!("ignoring {path} (no file extension)");
            }
            return None;
        };
        let Some(lang) = SupportedLanguage::try_from_extension(extension) else {
            if log_enabled!(log::Level::Trace) {
                trace!("ignoring {path} (no known language)");
            }
            return None;
        };
        Some(Self::load(path, lang))
    }

    fn load(path: PrettyPath, lang: SupportedLanguage) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path.as_ref())?;
        let tree = {
            let mut parser = Parser::new();
            parser
                .set_language(lang.ts_language())
                .with_context(|| format!("failed to load {} grammar", lang.name()))?;
            parser
                .parse(&content, None)
                .with_context(|| format!("failed to parse {path}"))?
        };
        Ok(Self {
            path,
            content,
            lang,
            tree,
        })
    }
}
