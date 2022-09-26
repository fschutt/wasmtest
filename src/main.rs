use wasmer::{Store, Module, Instance, Value, imports};
use wasmer_vfs::FileSystem;
use wasmer_wasi::{WasiFunctionEnv, WasiState};
use wasmer_vfs::mem_fs::FileSystem as MemFileSystem;
use std::collections::BTreeMap;
use std::path::{PathBuf, Path};
use wasmer_wasi::WasiBidirectionalSharedPipePair;

const PYTHON: &[u8] = include_bytes!("../python.tar.gz");
const TEST_SCRIPT: &str = include_str!("./test.py");

#[derive(Debug, Clone, PartialEq, Ord, Eq, PartialOrd)]
pub enum DirOrFile {
    File(PathBuf),
    Dir(PathBuf),
}

pub type FileMap = BTreeMap<DirOrFile, Vec<u8>>;

/// Unzips a .tar.gz file, returning the [FileName => FileContents]
pub fn unpack_tar_gz(bytes: Vec<u8>, prefix: &str) -> Result<FileMap, String> {
    use flate2::read::GzDecoder;
    use std::io::Cursor;
    use tar::{Archive, EntryType};

    let mut cursor = Cursor::new(bytes);
    let mut archive = Archive::new(GzDecoder::new(cursor));

    // TODO(felix): it would be ideal if the .tar.gz file could
    // be unpacked in-memory instead of requiring disk access.

    // Use a random directory name for unpacking: in case the
    // tool is ran in parallel, this would otherwise lead to
    // file conflicts
    let rand_dir = rand::random::<u64>();
    let tempdir = std::env::temp_dir()
        .join("wapm-to-webc")
        .join(&format!("{rand_dir}"));

    let _ = std::fs::remove_dir(&tempdir); // no error if dir doesn't exist
    let _ = std::fs::create_dir_all(&tempdir)
        .map_err(|e| format!("{}: {e}", tempdir.display()))?;

    let mut files = BTreeMap::default();

    for (i, file) in archive.entries().unwrap().enumerate() {
        let mut file = file.map_err(|e| format!("{}: {e}", tempdir.display()))?;

        let file_type = file.header().entry_type();

        let path = file
            .path()
            .map_err(|e| format!("{}: {e}", tempdir.display()))?
            .to_owned()
            .to_path_buf();

        let outpath = tempdir.clone().join(&format!("{i}.bin"));

        let _ = file
            .unpack(&outpath)
            .map_err(|e| format!("{}: {e}", outpath.display()))?;

        let path = match file_type {
            EntryType::Regular => DirOrFile::File(path),
            EntryType::Directory => DirOrFile::Dir(path),
            e => {
                return Err(format!(
                    "Invalid file_type for path \"{}\": {:?}",
                    path.display(),
                    e
                ));
            }
        };

        let bytes = match &path {
            DirOrFile::File(_) => std::fs::read(&outpath)
                .map_err(|e| format!("{}: {e}", outpath.display()))?,
            DirOrFile::Dir(_) => Vec::new(),
        };



        let path = match &path {
            DirOrFile::File(f) => {
                if !format!("{}", f.display()).starts_with(prefix) {
                    continue;
                }
                // python/atom/lib/
                DirOrFile::File(
                    Path::new(&format!("{}", f.display())
                    .replacen(prefix, "", 1)
                ).to_path_buf())
            },
            DirOrFile::Dir(d) => {
                if !format!("{}", d.display()).starts_with(prefix) {
                    continue;
                }
                // python/atom/lib/
                DirOrFile::Dir(
                    Path::new(&format!("{}", d.display())
                    .replacen(prefix, "", 1)
                ).to_path_buf())
            }
        };

        files.insert(path, bytes);
    }

    nuke_dir(tempdir)?;

    Ok(files)
}

fn nuke_dir<P: AsRef<Path>>(path: P) -> Result<(), String> {
    use std::fs;
    let path = path.as_ref();
    for entry in
        fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))?
    {
        let entry = entry.map_err(|e| format!("{}: {e}", path.display()))?;
        let path = entry.path();

        let file_type = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;

        if file_type.is_dir() {
            nuke_dir(&path)?;
            fs::remove_dir(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }

    Ok(())
}

pub(crate) fn exec_module(
    store: &mut Store,
    module: &Module,
    mut wasi_env: wasmer_wasi::WasiFunctionEnv,
) -> Result<(), String> {

    let import_object = wasi_env.import_object(store, &module)
        .map_err(|e| format!("{e}"))?;
    let instance = Instance::new(store, &module, &import_object)
        .map_err(|e| format!("{e}"))?;
    let memory = instance.exports.get_memory("memory")
        .map_err(|e| format!("{e}"))?;
    wasi_env.data_mut(store).set_memory(memory.clone());

    // If this module exports an _initialize function, run that first.
    if let Ok(initialize) = instance.exports.get_function("_initialize") {
        initialize
            .call(store, &[])
            .map_err(|e| format!("failed to run _initialize function: {e}"))?;
    }

    let result = instance.exports
        .get_function("_start")
        .map_err(|e| format!("{e}"))?
        .call(store, &[])
        .map_err(|e| format!("{e}"))?;

        Ok(())
}

fn prepare_webc_env(
    store: &mut Store,
    stdout: WasiBidirectionalSharedPipePair,
    files: &FileMap,
    command: &str,
) -> Result<WasiFunctionEnv, String> {
    let fs = MemFileSystem::default();
    for key in files.keys() {
        match key {
            DirOrFile::Dir(d) => { 
                let mut s = format!("{}", d.display());
                if s.is_empty() { continue; }
                let s = format!("/{s}");
                let _ = fs.create_dir(Path::new(&s)); 
            },
            DirOrFile::File(f) => {

            },
        }
    }
    for (k, v) in files.iter() {
        match k {
            DirOrFile::Dir(d) => { continue; },
            DirOrFile::File(d) => { 
                let mut s = format!("{}", d.display());
                if s.is_empty() { continue; }
                let s = format!("/{s}");
                let mut file = fs
                    .new_open_options()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .create(true)
                    .open(&Path::new(&s))
                    .unwrap();
                
                file.write(&v).unwrap();
            },
        }
    }

    let mut wasi_env = WasiState::new(command);
    wasi_env.set_fs(Box::new(fs));

    for key in files.keys() {
        let mut s = match key {
            DirOrFile::Dir(d) => format!("{}", d.display()),
            DirOrFile::File(f) => continue,
        };
        if s.is_empty() { continue; }
        let s = format!("/{s}");
        wasi_env.preopen(|p| {
            p.directory(&s).read(true).write(true).create(true)
        })
        .map_err(|e| format!("E4: {e}"))?;
    }

    Ok(
        wasi_env
        .env("PYTHONHOME", "/")
        .arg("/lib/file.py")
        .stdout(Box::new(stdout))
        .finalize(store)
        .map_err(|e| format!("E5: {e}"))?
    )
}

fn main() {
    use std::io::Read;

    let now = std::time::Instant::now();

    // Unpack python into memory and initialize file system
    let mut python_unpacked = unpack_tar_gz(PYTHON.to_vec(), "python/atom/").unwrap();
    let python_wasm = python_unpacked.remove(
        &DirOrFile::File(Path::new("lib/python.wasm").to_path_buf())
    ).unwrap();
    python_unpacked.insert(
        DirOrFile::File(Path::new("lib/file.py").to_path_buf()), 
        TEST_SCRIPT.as_bytes().to_vec()
    );

    let mut store = Store::default();
    let mut module = Module::from_binary(&store, &python_wasm).unwrap();
    module.set_name("python");

    let now2 = std::time::Instant::now();
    println!("compiled python.wasm and setup filesystem in {:?}", now2 - now);
    let now = std::time::Instant::now();

    let mut stdout_pipe = WasiBidirectionalSharedPipePair::new().with_blocking(false);
    
    let wasi_env = prepare_webc_env(
        &mut store, 
        stdout_pipe.clone(),
        &python_unpacked, 
        "python"
    ).unwrap();

    exec_module(&mut store, &module, wasi_env).unwrap();

    let mut buf = Vec::new();
    stdout_pipe.read_to_end(&mut buf).unwrap();
    let string = String::from_utf8_lossy(&buf);
    println!("output of script: {}", string);

    let now2 = std::time::Instant::now();
    println!("executed script in {:?}", now2 - now);
}