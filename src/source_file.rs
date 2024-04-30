use std::fs;

use dupe::Dupe;
use log::{info, log_enabled};
use tree_sitter::{Parser, Tree};

use crate::{
    error::{Error, IOAction},
    result::Result,
    source_path::SourcePath,
    supported_language::SupportedLanguage,
    trigger::{Trigger, TriggerCause},
};

#[derive(Debug)]
pub struct SourceFile {
    path: SourcePath,
    language: Option<SupportedLanguage>,
}

impl SourceFile {
    pub fn new(path: SourcePath) -> Result<Self> {
        let path = path.dupe();
        let language = path
            .abs_path
            .extension()
            .and_then(|extension| SupportedLanguage::try_from_extension(extension).ok());
        Ok(Self { path, language })
    }

    pub fn path(&self) -> &SourcePath {
        &self.path
    }

    pub fn parseable(&self) -> bool {
        self.language.is_some()
    }

    pub fn parse(&self) -> Result<ParsedSourceFile> {
        if log_enabled!(log::Level::Info) {
            info!("parsing {}", self.path);
        }
        let content =
            fs::read_to_string(self.path.abs_path.as_str()).map_err(|cause| Error::IO {
                path: self.path.pretty_path.dupe(),
                action: IOAction::Read,
                cause,
            })?;
        let Some(language) = self.language else {
            return Err(Error::Unparseable(self.path.pretty_path.dupe()));
        };
        let tree = {
            let mut parser = Parser::new();
            parser
                .set_language(language.ts_language())
                .map_err(Error::Language)?;
            let tree = parser
                .parse(&content, None)
                .expect("unexpected parser failure");
            if tree.root_node().has_error() {
                return Err(Error::UnparseableAsLanguage {
                    path: self.path.pretty_path.dupe(),
                    language,
                });
            }
            tree
        };
        let path = self.path.dupe();
        Ok(ParsedSourceFile {
            path,
            content,
            tree,
            language,
        })
    }
}

impl TriggerCause for SourceFile {
    fn matches(&self, trigger: &Trigger) -> bool {
        if let Some(content_trigger) = &trigger.content_trigger {
            if !self.language.is_some_and(|l| l == content_trigger.language) {
                return false;
            }
        }

        self.path.matches(trigger)
    }
}

#[derive(Debug)]
pub struct ParsedSourceFile {
    pub path: SourcePath,
    pub content: String,
    pub language: SupportedLanguage,
    pub tree: Tree,
}

impl PartialEq for ParsedSourceFile {
    fn eq(&self, other: &Self) -> bool {
        (&self.path, &self.content, self.language) == (&other.path, &other.content, other.language)
    }
}

impl Eq for ParsedSourceFile {}
