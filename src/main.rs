#![deny(missing_debug_implementations)]

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

mod cli;
mod context;
mod error;
mod irritation;
mod logger;
mod plural;
mod result;
mod scriptlets;
mod source_file;
mod source_path;
mod supported_language;
mod trigger;
mod verbosity;
mod vex;

#[cfg(test)]
mod vextest;

use std::{env, fs, process::ExitCode};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser as _;
use cli::{DumpCmd, ListCmd, MaxProblems, ToList};
use dupe::Dupe;
use lazy_static::lazy_static;
use log::{info, log_enabled, trace, warn};
use owo_colors::{OwoColorize, Stream, Style};
use source_file::SourceFile;
use strum::IntoEnumIterator;
use tree_sitter::QueryCursor;

use crate::{
    cli::{Args, CheckCmd, Command},
    context::Context,
    error::{Error, IOAction},
    irritation::Irritation,
    plural::Plural,
    result::Result,
    scriptlets::{
        event::{Event, MatchEvent, OpenFileEvent, OpenProjectEvent},
        Intent, PreinitingStore, QueryCaptures, VexingStore,
        query_cache::QueryCache,
    },
    source_path::{PrettyPath, SourcePath},
    supported_language::SupportedLanguage,
    trigger::FilePattern,
    verbosity::Verbosity,
};

// TODO(kcza): move the subcommands to separate files
fn main() -> ExitCode {
    match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = Args::parse();
    logger::init(Verbosity::try_from(args.verbosity_level)?)?;

    match args.command {
        Command::List(list_args) => list(list_args),
        Command::Check(cmd_args) => check(cmd_args),
        Command::Dump(dump_args) => dump(dump_args),
        Command::Init => init(),
    }?;

    Ok(logger::exit_code())
}

fn list(list_args: ListCmd) -> Result<()> {
    match list_args.what {
        ToList::Checks => {
            let ctx = Context::acquire()?;
            let store = PreinitingStore::new(&ctx)?.preinit()?;
            store
                .vexes()
                .flat_map(|vex| vex.path.pretty_path.file_stem())
                .for_each(|id| println!("{}", id));
        }
        ToList::Languages => SupportedLanguage::iter().for_each(|lang| println!("{}", lang)),
    }
    Ok(())
}

lazy_static! {
    static ref SUCCESS_STYLE: Style = Style::new().green().bold();
}

fn check(cmd_args: CheckCmd) -> Result<()> {
    let ctx = Context::acquire()?;
    let store = PreinitingStore::new(&ctx)?.preinit()?.init()?;

    let RunData {
        irritations,
        num_files_scanned,
    } = vex(&ctx, &store, cmd_args.max_problems)?;
    irritations.iter().for_each(|irr| println!("{irr}"));
    if log_enabled!(log::Level::Info) {
        info!(
            "scanned {}",
            Plural::new(num_files_scanned, "file", "files"),
        );
    }
    if !irritations.is_empty() {
        warn!(
            "found {}",
            Plural::new(irritations.len(), "problem", "problems"),
        );
    } else {
        println!(
            "{}: no problems found",
            "success".if_supports_color(Stream::Stdout, |text| text.style(*SUCCESS_STYLE))
        );
    }

    Ok(())
}

#[derive(Debug)]
struct RunData {
    irritations: Vec<Irritation>,
    num_files_scanned: usize,
}

impl RunData {
    #[cfg(test)]
    fn into_irritations(self) -> Vec<Irritation> {
        self.irritations
    }
}

fn vex(ctx: &Context, store: &VexingStore, max_problems: MaxProblems) -> Result<RunData> {
    let files = {
        let mut paths = Vec::new();
        let ignores = ctx
            .ignores
            .clone()
            .into_inner()
            .into_iter()
            .map(|ignore| ignore.compile(&ctx.project_root))
            .collect::<Result<Vec<_>>>()?;
        let allows = ctx
            .allows
            .clone()
            .into_iter()
            .map(|allow| allow.compile(&ctx.project_root))
            .collect::<Result<Vec<_>>>()?;
        walkdir(
            ctx,
            ctx.project_root.as_ref(),
            &ignores,
            &allows,
            &mut paths,
        )?;
        paths
            .into_iter()
            .map(|p| SourcePath::new(&p, &ctx.project_root))
            .map(SourceFile::new)
            .collect::<Result<Vec<_>>>()?
    };

    let project_queries_hint = store.project_queries_hint();
    let file_queries_hint = store.file_queries_hint();

    let query_cache = QueryCache::with_capacity(project_queries_hint + file_queries_hint);

    let mut irritations = vec![];
    let frozen_heap = store.frozen_heap();
    let project_queries = {
        let mut project_queries = Vec::with_capacity(project_queries_hint);
        let path = ctx.project_root.dupe();
        store
            .observer_data()
            .handle(Event::OpenProject(OpenProjectEvent::new(path)), &query_cache, frozen_heap)?
            .iter()
            .for_each(|intent| match intent {
                Intent::Find {
                    language,
                    query,
                    on_match,
                } => project_queries.push((*language, query.dupe(), on_match.dupe())),
                Intent::Observe { .. } => panic!("internal error: non-init observe"),
                Intent::Warn(irr) => irritations.push(irr.clone()),
            });
        project_queries
    };

    for file in &files {
        let Some(language) = file.language() else {
            if log_enabled!(log::Level::Info) {
                info!("skipping {}", file.path());
            }
            continue;
        };

        let file_queries = {
            let mut file_queries = Vec::with_capacity(store.file_queries_hint());
            let path = file.path().pretty_path.dupe();
            store
                .observer_data()
                .handle(Event::OpenFile(OpenFileEvent::new(path)), &query_cache, frozen_heap)?
                .iter()
                .for_each(|intent| match intent {
                    Intent::Find {
                        language,
                        query,
                        on_match,
                    } => file_queries.push((*language, query.dupe(), on_match.dupe())),
                    Intent::Observe { .. } => panic!("internal error: non-init observe"),
                    Intent::Warn(irr) => irritations.push(irr.clone()),
                });
            file_queries
        };

        if project_queries
            .iter()
            .chain(file_queries.iter())
            .all(|(l, _, _)| *l != language)
        {
            continue; // No need to parse, the user will never search this.
        }
        let parsed_file = file.parse()?;
        project_queries
            .iter()
            .chain(file_queries.iter())
            .filter(|(l, _, _)| *l == language)
            .try_for_each(|(_, query, on_match)| {
                QueryCursor::new()
                    .matches(
                        query,
                        parsed_file.tree.root_node(),
                        parsed_file.content.as_bytes(),
                    )
                    .try_for_each(|qmatch| {
                        let event = {
                            let path = &parsed_file.path.pretty_path;
                            let captures = QueryCaptures::new(query, &qmatch, &parsed_file);
                            Event::Match(MatchEvent::new(path.dupe(), captures))
                        };
                        on_match.handle(event, &query_cache, frozen_heap)?.iter().for_each(
                            |intent| match intent {
                                Intent::Find { .. } => {
                                    panic!("internal error: find intended during find")
                                }
                                Intent::Observe { .. } => {
                                    panic!("internal error: non-init observe")
                                }
                                Intent::Warn(irr) => irritations.push(irr.clone()),
                            },
                        );

                        Ok::<_, Error>(())
                    })
            })?;
    }

    irritations.sort();
    if let MaxProblems::Limited(max) = max_problems {
        let max = max as usize;
        if max < irritations.len() {
            irritations.truncate(max);
        }
    }
    Ok(RunData {
        irritations,
        num_files_scanned: files.len(),
    })
}

fn walkdir(
    ctx: &Context,
    path: &Utf8Path,
    ignores: &[FilePattern],
    allows: &[FilePattern],
    paths: &mut Vec<Utf8PathBuf>,
) -> Result<()> {
    if log_enabled!(log::Level::Trace) {
        trace!("walking {path}");
    }
    let entries = fs::read_dir(path).map_err(|cause| Error::IO {
        path: PrettyPath::new(path),
        action: IOAction::Read,
        cause,
    })?;
    for entry in entries {
        let entry = entry.map_err(|cause| Error::IO {
            path: PrettyPath::new(path),
            action: IOAction::Read,
            cause,
        })?;
        let entry_path = Utf8PathBuf::try_from(entry.path())?;
        let metadata = fs::symlink_metadata(&entry_path).map_err(|cause| Error::IO {
            path: PrettyPath::new(&entry_path),
            action: IOAction::Read,
            cause,
        })?;
        let is_dir = metadata.is_dir();

        let project_relative_path =
            Utf8Path::new(&entry_path.as_str()[ctx.project_root.as_str().len()..]);
        if !allows.iter().any(|p| p.matches(project_relative_path)) {
            let hidden = project_relative_path
                .file_name()
                .is_some_and(|name| name.starts_with('.'));
            if hidden || ignores.iter().any(|p| p.matches(project_relative_path)) {
                if log_enabled!(log::Level::Info) {
                    let dir_marker = if is_dir { "/" } else { "" };
                    info!("ignoring {project_relative_path}{dir_marker}");
                }
                continue;
            }
        }

        if metadata.is_symlink() {
            if log_enabled!(log::Level::Info) {
                let symlink_path = entry_path.strip_prefix(ctx.project_root.as_ref())?;
                info!("ignoring /{symlink_path} (symlink)");
            }
        } else if is_dir {
            walkdir(ctx, &entry_path, ignores, allows, paths)?;
        } else if metadata.is_file() {
            paths.push(entry_path);
        } else {
            panic!("unreachable");
        }
    }

    Ok(())
}

fn dump(dump_args: DumpCmd) -> Result<()> {
    let cwd = Utf8PathBuf::try_from(env::current_dir().map_err(|e| Error::IO {
        path: PrettyPath::new(Utf8Path::new(&dump_args.path)),
        action: IOAction::Read,
        cause: e,
    })?)?;
    let src_path = SourcePath::new_in(&dump_args.path, &cwd);
    let src_file = SourceFile::new(src_path)?.parse()?;
    println!("{}", src_file.tree.root_node().to_sexp());

    Ok(())
}

fn init() -> Result<()> {
    let cwd = Utf8PathBuf::try_from(env::current_dir().map_err(|cause| Error::IO {
        path: PrettyPath::new(Utf8Path::new(".")),
        action: IOAction::Read,
        cause,
    })?)?;
    Context::init(cwd)?;
    let queries_dir = Context::acquire()?.manifest.queries_dir;
    println!(
        "{}: vex initialised, now add style rules in ./{}/",
        "success".if_supports_color(Stream::Stdout, |text| text.style(*SUCCESS_STYLE)),
        queries_dir.as_str(),
    );
    Ok(())
}

#[cfg(test)]
mod test {
    use std::{fs::File, io::Write, path};

    use indoc::indoc;
    use insta::assert_yaml_snapshot;
    use joinery::JoinableIterator;
    use tempfile::TempDir;

    use crate::vextest::VexTest;

    use super::*;

    struct TestFile {
        _dir: TempDir,
        path: Utf8PathBuf,
    }

    impl TestFile {
        fn new(path: impl AsRef<str>, content: impl AsRef<[u8]>) -> TestFile {
            let dir = tempfile::tempdir().unwrap();
            let file_path = Utf8PathBuf::try_from(dir.path().to_path_buf())
                .unwrap()
                .join(path.as_ref());

            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            File::create(&file_path)
                .unwrap()
                .write_all(content.as_ref())
                .unwrap();

            TestFile {
                _dir: dir,
                path: file_path,
            }
        }
    }

    #[test]
    fn dump_valid_file() {
        let test_file = TestFile::new(
            "path/to/file.rs",
            indoc! {r#"
                fn add(a: i32, b: i32) -> i32 {
                    a + b
                }
            "#},
        );

        let args = Args::try_parse_from(["vex", "dump", test_file.path.as_str()]).unwrap();
        let cmd = args.command.into_dump_cmd().unwrap();
        dump(cmd).unwrap();
    }

    #[test]
    fn dump_nonexistent_file() {
        let file_path = "/i/do/not/exist.rs";
        let args = Args::try_parse_from(["vex", "dump", file_path]).unwrap();
        let cmd = args.command.into_dump_cmd().unwrap();
        let err = dump(cmd).unwrap_err();
        if cfg!(target_os = "windows") {
            assert_eq!(
                err.to_string(),
                "cannot read /i/do/not/exist.rs: The system cannot find the path specified. (os error 3)"
            );
        } else {
            assert_eq!(
                err.to_string(),
                "cannot read /i/do/not/exist.rs: No such file or directory (os error 2)"
            );
        }
    }

    #[test]
    fn dump_invalid_file() {
        let test_file = TestFile::new(
            "src/file.rs",
            indoc! {r#"
                i am not valid a valid rust file!
            "#},
        );
        let args = Args::try_parse_from(["vex", "dump", test_file.path.as_str()]).unwrap();
        let cmd = args.command.into_dump_cmd().unwrap();
        let err = dump(cmd).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "cannot parse {} as rust",
                test_file.path.as_str().replace(path::MAIN_SEPARATOR, "/")
            )
        );
    }

    #[test]
    fn no_extension() {
        let test_file = TestFile::new("no-extension", "");
        let args = Args::try_parse_from(["vex", "dump", test_file.path.as_str()]).unwrap();
        let cmd = args.command.into_dump_cmd().unwrap();
        let err = dump(cmd).unwrap_err();

        // Assertion relaxed due to strange Github Actions Windows and Macos runner path handling.
        let expected = format!("cannot parse {}", PrettyPath::new(&test_file.path));
        assert!(
            err.to_string().ends_with(&expected),
            "unexpected error: expected {expected} but got {err}"
        );
    }

    #[test]
    fn unknown_extension() {
        let test_file = TestFile::new("file.unknown-extension", "");
        let args = Args::try_parse_from(["vex", "dump", test_file.path.as_str()]).unwrap();
        let cmd = args.command.into_dump_cmd().unwrap();
        let err = dump(cmd).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("cannot parse {}", PrettyPath::new(&test_file.path))
        );
    }

    #[test]
    fn max_problems() {
        const MAX: u32 = 47;
        let irritations = VexTest::new("max-problems")
            .with_max_problems(MaxProblems::Limited(MAX))
            .with_scriptlet(
                "vexes/var.star",
                indoc! {r#"
                    def init():
                        vex.observe('open_project', on_open_project)

                    def on_open_project(event):
                        vex.search(
                            'rust',
                            '(integer_literal) @num',
                            on_match,
                        )

                    def on_match(event):
                        vex.warn('oh no a number!', at=(event.captures['num'], 'num'))
                "#},
            )
            .with_source_file(
                "src/main.rs",
                indoc! {r#"
                    fn main() {
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        let x = 1 + 2 + 3 + 4 + 5 + 6 + 8 + 9 + 10;
                        println!("{x}");
                    }
                "#},
            )
            .try_run()
            .unwrap()
            .into_irritations();
        assert_eq!(irritations.len(), MAX as usize);
    }

    #[test]
    fn readme() {
        // Dumb hacky test to serve until mdbook docs are made and tested.
        let collate_snippets = |language| {
            include_str!("../README.md")
                .lines()
                .scan(false, |collate_starlark, line| {
                    Some(if let Some(stripped) = line.strip_prefix("```") {
                        *collate_starlark = stripped.starts_with(language);
                        None
                    } else if *collate_starlark {
                        Some(line)
                    } else {
                        None
                    })
                })
                .flatten()
                .join_with("\n")
                .to_string()
        };
        let collated_starlark_snippets = collate_snippets("python");
        let collated_rust_snippets = collate_snippets("rust");
        let irritations = VexTest::new("README-snippets")
            .with_scriptlet("vexes/test.star", collated_starlark_snippets)
            .with_source_file("src/main.rs", collated_rust_snippets)
            .try_run()
            .unwrap()
            .into_irritations()
            .into_iter()
            .map(|irr| irr.to_string())
            .collect::<Vec<_>>();
        assert_yaml_snapshot!(irritations);
    }
}
