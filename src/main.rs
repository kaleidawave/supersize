use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder};
use colored::Colorize;
use futures::future::join_all;
use humansize::{file_size_opts, FileSize};
use once_cell::sync::OnceCell;
use std::{
    env,
    fmt::Display,
    path::{Path, PathBuf},
};
use tokio::{io::AsyncReadExt, try_join};
use wax::Pattern;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut cli_args = env::args_os().skip(1);

    let mut filter = None::<FileFilter>;
    // The first path is "eaten" (pulled out of iterator) from flag scanning. So is stored here to later rejoin.
    let mut first_path = None;

    while let Some(arg) = cli_args.next() {
        if let Some(flag) = arg.to_str().and_then(|arg_str| arg_str.strip_prefix("--")) {
            let arg = cli_args.next();

            let arg = if let Some(string) = arg.and_then(|os_str| os_str.into_string().ok()) {
                Box::<str>::leak(string.into_boxed_str())
            } else {
                eprintln!("Expected value for flag {:?}", flag);
                return;
            };
            match flag {
                "include" => {
                    let glob = if let Ok(glob) = wax::Glob::new(arg) {
                        glob
                    } else {
                        eprintln!("Invalid glob pattern");
                        return;
                    };
                    filter = Some(FileFilter {
                        mode: FilterMode::Include,
                        glob,
                    });
                }
                "exclude" => {
                    let glob = if let Ok(glob) = wax::Glob::new(arg) {
                        glob
                    } else {
                        eprintln!("Invalid glob pattern");
                        return;
                    };
                    filter = Some(FileFilter {
                        mode: FilterMode::Exclude,
                        glob,
                    });
                }
                flag => {
                    eprintln!("Unknown flag {:?}", flag);
                    return;
                }
            }
        } else {
            first_path = Some(arg);
            break;
        }
    }

    if first_path.is_none() {
        eprintln!("No files or folders specified as arguments");
        return;
    }

    let file_folder = first_path.into_iter().chain(cli_args);

    let mut paths =
        join_all(file_folder.map(|argument| read_path(PathBuf::from(argument), &filter))).await;

    // Initial info (could run before everything has resolved)
    for path in paths.iter() {
        println!("{}: {}", "Path".bold(), path.path().display());
        path.total_size().display();
    }

    // Sort them by size
    paths.sort_unstable_by_key(|path| path.total_size().uncompressed);
    let mut paths = paths.into_iter();

    // Summary
    if let Some(first_path) = paths.next() {
        let first_path_name = first_path.path();
        let first_path_size = first_path.total_size().uncompressed;
        
        println!(
            "\n{} {} {}",
            "Smallest:".bold(),
            first_path_name.display().to_string().bold().bright_green(),
            file_size_to_string(first_path_size)
        );

        if paths.len() > 0 {
            println!("\n{}", "Other paths:".bold());
        }

        for path in paths {
            let path_size = path.total_size().uncompressed;
            let scale = path_size as f64 / first_path_size as f64;
            println!(
                "    {} {}, {:.2} times larger that {}",
                path.path().display().to_string().bright_magenta(),
                file_size_to_string(path.total_size().uncompressed),
                scale,
                first_path_name.display().to_string().bright_green()
            )
        }
    }
}

#[derive(Clone)]
struct FileSizeInfo {
    pub uncompressed: usize,
    pub gzip: Option<usize>,
    pub brotli: Option<usize>,
}

fn file_size_to_string(size: usize) -> impl Display {
    size.file_size(file_size_opts::DECIMAL)
        .unwrap()
        .bold()
        .bright_yellow()
}

impl FileSizeInfo {
    fn display(&self) {
        let Self {
            uncompressed,
            gzip,
            brotli,
        } = self;
        print!("    Size: {}", file_size_to_string(*uncompressed));
        if let Some(gzip) = gzip {
            print!(" ({} gzip)", file_size_to_string(*gzip));
        }
        if let Some(brotli) = brotli {
            print!(" ({} brotli)", file_size_to_string(*brotli));
        }
        println!();
    }
}

enum PathData {
    File {
        name: PathBuf,
        size: FileSizeInfo,
    },
    Folder {
        path: PathBuf,
        files: Box<[PathData]>,
        total_size: OnceCell<FileSizeInfo>,
    },
}

impl PathData {
    fn total_size(&self) -> &FileSizeInfo {
        match self {
            PathData::Folder {
                total_size, files, ..
            } => total_size.get_or_init(|| {
                let iter = files.iter().map(|file| file.total_size().clone());
                // Sum file sizes
                iter.reduce(|existing, file| FileSizeInfo {
                    uncompressed: existing.uncompressed + file.uncompressed,
                    gzip: existing.gzip.zip(file.gzip).map(|(a, b)| a + b),
                    brotli: existing.brotli.zip(file.brotli).map(|(a, b)| a + b),
                })
                .unwrap_or(FileSizeInfo {
                    uncompressed: 0,
                    gzip: None,
                    brotli: None,
                })
            }),
            PathData::File { size, .. } => size,
        }
    }

    fn path(&self) -> &Path {
        match self {
            PathData::Folder { path, .. } | PathData::File { name: path, .. } => path,
        }
    }
}

enum FilterMode {
    Include,
    Exclude,
}

struct FileFilter {
    mode: FilterMode,
    glob: wax::Glob<'static>,
}

impl FileFilter {
    fn include_path(&self, path: &Path) -> bool {
        let glob_matches = self.glob.is_match(path);
        match self.mode {
            FilterMode::Include => glob_matches,
            FilterMode::Exclude => !glob_matches,
        }
    }
}

async fn get_file(path: &Path) -> tokio::io::Result<(PathBuf, FileSizeInfo)> {
    let content = tokio::fs::read(path).await?;

    let mut gzip_encoder = GzipEncoder::new(content.as_slice());
    let mut gzip_buffer = Vec::new();
    let gzip_len = gzip_encoder.read_to_end(&mut gzip_buffer);

    let mut brotli_encoder = BrotliEncoder::new(content.as_slice());
    let mut brotli_buffer = Vec::new();
    let brotli_len = brotli_encoder.read_to_end(&mut brotli_buffer);

    let (gzip_len, brotli_len) = try_join!(gzip_len, brotli_len).unwrap();

    Ok((
        path.to_owned(),
        FileSizeInfo {
            uncompressed: content.len(),
            gzip: Some(gzip_len),
            brotli: Some(brotli_len),
        },
    ))
}

#[async_recursion::async_recursion]
async fn read_path(path: PathBuf, filter: &Option<FileFilter>) -> PathData {
    if !path.exists() {
        todo!("Could not find file, (should throw error)")
    }
    if path.is_dir() {
        let path_read_futures = path.read_dir().unwrap().filter_map(|argument| {
            let path = argument.unwrap().path();
            if filter
                .as_ref()
                .map(|filter| filter.include_path(&path))
                .unwrap_or(true)
            {
                Some(read_path(path, filter))
            } else {
                None
            }
        });
        let paths = join_all(path_read_futures).await;

        PathData::Folder {
            path: path.to_owned(),
            files: paths.into_boxed_slice(),
            total_size: OnceCell::new(),
        }
    } else {
        let (name, size) = get_file(&path).await.unwrap();
        PathData::File { name, size }
    }
}
