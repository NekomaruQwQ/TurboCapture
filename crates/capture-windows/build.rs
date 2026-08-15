//! Compile the fixed-frame shaders as part of the ordinary Cargo build.

use std::{
    env,
    error::Error,
    fs,
    io,
    path::{Path, PathBuf},
    process::Command,
};

/// Build both Shader Model 5 entry points consumed by `frame.rs`.
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=src/fixed_frame.hlsl");
    let output_directory = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("Cargo did not provide OUT_DIR"))?;
    let compiler = find_fxc()?;
    let source = Path::new("src/fixed_frame.hlsl");
    compile_shader(&compiler, source, &output_directory, "vs_main", "vs_5_0")?;
    compile_shader(&compiler, source, &output_directory, "ps_main", "ps_5_0")?;
    Ok(())
}

/// Locate `fxc.exe` from an override, `PATH`, or the newest installed Windows SDK.
fn find_fxc() -> io::Result<PathBuf> {
    if let Some(override_path) = env::var_os("FXC") {
        return Ok(PathBuf::from(override_path));
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let compiler = directory.join("fxc.exe");
            if compiler.is_file() {
                return Ok(compiler);
            }
        }
    }

    let program_files = env::var_os("ProgramFiles(x86)")
        .or_else(|| env::var_os("ProgramFiles"))
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::NotFound,
            "neither ProgramFiles(x86) nor ProgramFiles is defined"))?;
    let sdk_bin = PathBuf::from(program_files).join("Windows Kits/10/bin");
    let architecture = match env::consts::ARCH {
        "aarch64" => "arm64",
        "x86" => "x86",
        _ => "x64",
    };
    let mut versions = fs::read_dir(&sdk_bin)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
        .collect::<Vec<_>>();
    versions.sort_by_key(fs::DirEntry::file_name);
    versions.reverse();
    for version in versions {
        let compiler = version.path().join(architecture).join("fxc.exe");
        if compiler.is_file() {
            return Ok(compiler);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "fxc.exe was not found; install the Windows SDK or set FXC"))
}

/// Invoke the established `fxc` toolchain for one checked source entry point.
fn compile_shader(
    compiler: &Path,
    source: &Path,
    output_directory: &Path,
    entry_point: &str,
    target_profile: &str) -> Result<(), Box<dyn Error>> {
    let output = output_directory.join(format!("fixed_frame_{entry_point}.fxo"));
    let status = Command::new(compiler)
        .args([
            "/nologo",
            "/O3",
            "/Zi",
            "/WX",
            "/T",
            target_profile,
            "/E",
            entry_point,
            "/Fo"])
        .arg(&output)
        .arg(source)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "fxc failed for {entry_point} with {status}"))
            .into());
    }
    Ok(())
}
