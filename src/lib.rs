// The shader wrangler receives a source dir, a target dir, a rename policy, and a list of kinds of
// shaders to compile.  It compiles via shaderc and looks for files with glob.

use serde::{Deserialize, Serialize};
use shaderc;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use thiserror::Error;

pub use shaderc::ShaderKind;

/// Errors that `wrangler` might encounter during compilation.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Kind {0:?} not supported by wrangler")]
    UnsupportedKind(ShaderKind),
    #[error("Bad glob pattern: `{0}`")]
    BadGlobPattern(String),
    #[error("Error while traversing glob results: {0:?}")]
    GlobTraversal(#[from] glob::GlobError),
    #[error("IO error: {0:?}")]
    Io(#[from] std::io::Error),
    #[error("Error initializing the shaderc compiler")]
    CompilerInit,
    #[error("Error compiling file to SPIR-V: {0:?}")]
    Compilation(#[from] shaderc::Error),
    #[error("Encountered errors compiling some files: {0:?}")]
    BatchError(Vec<Error>),
}

/// Specifies a couple behaviors of the `run` function.
pub struct Instructions {
    /// The types of shaders we are to search for and compile.
    pub to_compile: Vec<ShaderKind>,
    /// We assume `search_dir` contains a valid path, in order to not make the
    /// user go through the trouble of converting into a Path or PathBuf.
    pub search_root: &'static str,
    pub output_root: &'static str,
    pub record_path: &'static str,
    /// If true, `run()` will terminate with an `Err` value if one or more files
    /// fails to compile.  Otherwise we print a warning describing which files
    /// failed and how.
    pub compilation_error_terminates: bool,
}

fn deduplicate_kinds(kinds: &Vec<ShaderKind>) -> Vec<ShaderKind> {
    let mut out = Vec::<ShaderKind>::new();
    'over_kinds: for kind in kinds.iter() {
        for added in out.iter() {
            if *added == *kind {
                continue 'over_kinds;
            }
        }
        out.push(kind.clone())
    }
    out
}

#[derive(Clone)]
struct CompilationCandidate {
    location: PathBuf,
    shader_kind: ShaderKind,
}

#[derive(Serialize, Deserialize)]
struct Record {
    modified_times: HashMap<PathBuf, SystemTime>,
}

impl Record {
    fn try_load(instructions: &Instructions) -> Result<Record> {
        let path: PathBuf = instructions.record_path.into();
        if path.exists() {
            let f = fs::File::open(path)?;
            if let Ok(record) = rmp_serde::from_read(f) {
                return Ok(record);
            }
        }
        Ok(Record {
            modified_times: HashMap::new(),
        })
    }

    fn log(&mut self, file: impl AsRef<Path>) -> Result<()> {
        let file: &Path = file.as_ref();
        let metadata = fs::metadata(&file)?;
        let modified = metadata.modified()?;
        self.modified_times.insert(file.to_owned(), modified);
        Ok(())
    }

    fn write(&self, instructions: &Instructions) -> Result<()> {
        let path: PathBuf = instructions.record_path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(path)?;
        rmp_serde::encode::write(&mut file, self).unwrap();
        Ok(())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn kind_ext(kind: &ShaderKind) -> Result<&'static str> {
    match kind {
        ShaderKind::Vertex => Ok("vert"),
        ShaderKind::Fragment => Ok("frag"),
        ShaderKind::Compute => Ok("comp"),
        x => Err(Error::UnsupportedKind(x.clone())),
    }
}

fn find_shaders_of_kind(
    kind: &ShaderKind,
    search_root: &'static str,
) -> Result<Vec<CompilationCandidate>> {
    let pattern = format!("{}/**/*.{}", search_root, kind_ext(kind)?);
    let glob = glob::glob(&pattern).map_err(|_| Error::BadGlobPattern(pattern))?;
    glob.into_iter()
        .map(|x| {
            x.map(|path| CompilationCandidate {
                location: path,
                shader_kind: kind.clone(),
            })
            .map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()
}

fn find_shaders(instructions: &Instructions) -> Result<Vec<CompilationCandidate>> {
    let kinds = deduplicate_kinds(&instructions.to_compile);
    let mut shaders = Vec::<CompilationCandidate>::new();
    for kind in kinds {
        shaders.extend(find_shaders_of_kind(&kind, instructions.search_root)?.into_iter())
    }
    Ok(shaders)
}

fn check_against_record(
    candidates: &Vec<CompilationCandidate>,
    record: &Record,
) -> Result<Vec<CompilationCandidate>> {
    let mut needs_compile = Vec::<CompilationCandidate>::new();
    for candidate in candidates.iter() {
        if let Some(&last_modified) = record.modified_times.get(&candidate.location) {
            let file_modified = fs::metadata(candidate.location.clone())?.modified()?;
            if last_modified != file_modified {
                needs_compile.push(candidate.clone())
            }
        } else {
            needs_compile.push(candidate.clone());
        }
    }
    Ok(needs_compile)
}

struct CompileOutput {
    location: PathBuf,
    shader_kind: ShaderKind,
    artifact: shaderc::CompilationArtifact,
}

fn compile(to_compile: &Vec<CompilationCandidate>) -> Vec<Result<CompileOutput>> {
    // If shaderc can't run on this machine, there's not much we can do here.
    let mut compiler = shaderc::Compiler::new().unwrap();
    let mut out = Vec::<Result<CompileOutput>>::new();
    for CompilationCandidate {
        location,
        shader_kind,
    } in to_compile.iter()
    {
        let r: Result<_> = fs::File::open(location)
            .and_then(|mut f| {
                let mut s = String::new();
                f.read_to_string(&mut s).map(|_| s)
            })
            .map_err(Into::into)
            .and_then(|contents| {
                let location = location.to_str().unwrap();
                compiler
                    .compile_into_spirv(contents.as_str(), *shader_kind, location, "main", None)
                    .map_err(Into::into)
            })
            .map(|artifact| CompileOutput {
                location: location.clone(),
                shader_kind: *shader_kind,
                artifact,
            });
        out.push(r)
    }
    out
}

fn write_output(instructions: &Instructions, out: &CompileOutput) -> Result<()> {
    let extension = format!("spv_{}", kind_ext(&out.shader_kind)?);
    let tail = out.location.strip_prefix(instructions.search_root).unwrap();
    let mut dest: PathBuf = std::path::PathBuf::from(instructions.output_root).join(tail);
    dest.set_extension(extension);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = fs::File::create(dest)?;
    f.write(out.artifact.as_binary_u8())?;
    Ok(())
}

pub fn run(instructions: Instructions) -> Result<()> {
    let compile_candidates = find_shaders(&instructions)?;
    let mut record = Record::try_load(&instructions)?;
    let to_compile = check_against_record(&compile_candidates, &record)?;
    // GTFO now so we don't waste time loading shaderc if we have no use for it
    if to_compile.is_empty() {
        return Ok(());
    }
    let compilation_results = compile(&to_compile);
    for result in compilation_results.iter() {
        match result {
            Ok(output) => {
                write_output(&instructions, output)?;
                record.log(&output.location)?;
            }
            Err(_) => {
                // TODO: write error here
            }
        }
    }
    record.write(&instructions)?;
    let errors = compilation_results
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if !errors.is_empty() && instructions.compilation_error_terminates {
        return Err(Error::BatchError(errors));
    }
    Ok(())
}
