// Copyright 2024 KVCache.AI
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg_attr(test, allow(dead_code))]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const CMAKE_CONFIGURATION_SUFFIXES: [(&str, &str); 4] = [
    ("Debug", "DEBUG"),
    ("Release", "RELEASE"),
    ("RelWithDebInfo", "RELWITHDEBINFO"),
    ("MinSizeRel", "MINSIZEREL"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstrumentationConfig {
    asan: bool,
    gcov: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedCmakeCache {
    build_dir: PathBuf,
    cache_path: PathBuf,
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn canonical_existing_dir(label: &str, path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("{label} '{}' cannot be resolved: {error}", path.display()))?;
    if !fs::metadata(&canonical)
        .map_err(|error| {
            format!(
                "{label} '{}' cannot be inspected: {error}",
                canonical.display()
            )
        })?
        .is_dir()
    {
        return Err(format!(
            "{label} '{}' is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn derived_build_dir_from_store_lib_dir(store_lib_dir: &Path) -> Result<PathBuf, String> {
    let canonical = canonical_existing_dir("MOONCAKE_STORE_LIB_DIR", store_lib_dir)?;
    canonical
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "MOONCAKE_STORE_LIB_DIR '{}' does not identify a Mooncake build root",
                canonical.display()
            )
        })
}

fn selected_cmake_cache(
    manifest_dir: &Path,
    configured_build_dir: Option<PathBuf>,
    configured_store_lib_dir: Option<PathBuf>,
) -> Result<SelectedCmakeCache, String> {
    let explicit_build_dir = configured_build_dir
        .as_deref()
        .map(|path| canonical_existing_dir("MOONCAKE_BUILD_DIR", path))
        .transpose()?;
    let derived_build_dir = configured_store_lib_dir
        .as_deref()
        .map(derived_build_dir_from_store_lib_dir)
        .transpose()?;

    let build_dir = match (explicit_build_dir, derived_build_dir) {
        (Some(explicit), Some(derived)) => {
            if explicit != derived {
                return Err(format!(
                    "MOONCAKE_BUILD_DIR '{}' does not match the build root '{}' derived from MOONCAKE_STORE_LIB_DIR",
                    explicit.display(),
                    derived.display()
                ));
            }
            explicit
        }
        (Some(explicit), None) => explicit,
        (None, Some(derived)) => derived,
        (None, None) => canonical_existing_dir(
            "default Mooncake build directory",
            &manifest_dir.join("../../build"),
        )?,
    };
    let cache_path = build_dir.join("CMakeCache.txt");
    let metadata = fs::symlink_metadata(&cache_path).map_err(|error| {
        format!(
            "selected Mooncake CMake cache '{}' is unavailable: {error}",
            cache_path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "selected Mooncake CMake cache '{}' is not a regular file",
            cache_path.display()
        ));
    }
    Ok(SelectedCmakeCache {
        build_dir,
        cache_path,
    })
}

fn relevant_cache_key(key: &str) -> bool {
    matches!(
        key,
        "ENABLE_ASAN"
            | "CMAKE_BUILD_TYPE"
            | "CMAKE_C_FLAGS"
            | "CMAKE_CXX_FLAGS"
            | "CMAKE_C_FLAGS_DEBUG"
            | "CMAKE_CXX_FLAGS_DEBUG"
            | "CMAKE_C_FLAGS_RELEASE"
            | "CMAKE_CXX_FLAGS_RELEASE"
            | "CMAKE_C_FLAGS_RELWITHDEBINFO"
            | "CMAKE_CXX_FLAGS_RELWITHDEBINFO"
            | "CMAKE_C_FLAGS_MINSIZEREL"
            | "CMAKE_CXX_FLAGS_MINSIZEREL"
    )
}

fn expected_cache_type(key: &str) -> &'static str {
    if key == "ENABLE_ASAN" {
        "BOOL"
    } else {
        "STRING"
    }
}

fn parse_relevant_cmake_cache(contents: &str) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let left = trimmed
            .split_once('=')
            .map(|(left, _)| left)
            .unwrap_or(trimmed);
        let key = left.split_once(':').map(|(key, _)| key).unwrap_or(left);
        if !relevant_cache_key(key) {
            continue;
        }
        let (left, value) = trimmed
            .split_once('=')
            .ok_or_else(|| format!("CMake cache line {line_number} for {key} is malformed"))?;
        let (key, value_type) = left.split_once(':').ok_or_else(|| {
            format!("CMake cache line {line_number} for {key} is missing its type")
        })?;
        if value_type != expected_cache_type(key) {
            return Err(format!(
                "CMake cache line {line_number} for {key} has type {value_type}, expected {}",
                expected_cache_type(key)
            ));
        }
        if values.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!("CMake cache contains duplicate {key} entries"));
        }
    }
    Ok(values)
}

fn required_cache_value<'a>(
    values: &'a BTreeMap<String, String>,
    key: &str,
) -> Result<&'a str, String> {
    values
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("selected Mooncake CMake cache is missing required {key}"))
}

fn parse_cmake_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_uppercase().as_str() {
        "ON" | "TRUE" | "1" => Ok(true),
        "OFF" | "FALSE" | "0" => Ok(false),
        _ => Err(format!(
            "selected Mooncake CMake cache has invalid {key}={value:?}"
        )),
    }
}

fn selected_configuration_suffix(value: &str) -> Result<Option<&'static str>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    CMAKE_CONFIGURATION_SUFFIXES
        .iter()
        .find_map(|(name, suffix)| (*name == value).then_some(*suffix))
        .map(Some)
        .ok_or_else(|| {
            format!(
                "selected Mooncake CMake cache has unsupported or case-ambiguous CMAKE_BUILD_TYPE={value:?}"
            )
        })
}

fn contains_coverage_flag(value: &str) -> bool {
    ["--coverage", "-fprofile-arcs", "-ftest-coverage"]
        .iter()
        .any(|marker| value.contains(marker))
}

fn contains_asan_flag(value: &str) -> bool {
    value.contains("-fsanitize=address") || value.contains("-fsanitize=leak")
}

fn configured_instrumentation(contents: &str) -> Result<InstrumentationConfig, String> {
    let values = parse_relevant_cmake_cache(contents)?;
    let asan = parse_cmake_bool("ENABLE_ASAN", required_cache_value(&values, "ENABLE_ASAN")?)?;
    let configuration =
        selected_configuration_suffix(required_cache_value(&values, "CMAKE_BUILD_TYPE")?)?;
    let mut flags = vec![
        required_cache_value(&values, "CMAKE_C_FLAGS")?,
        required_cache_value(&values, "CMAKE_CXX_FLAGS")?,
    ];
    if let Some(suffix) = configuration {
        flags.push(required_cache_value(
            &values,
            &format!("CMAKE_C_FLAGS_{suffix}"),
        )?);
        flags.push(required_cache_value(
            &values,
            &format!("CMAKE_CXX_FLAGS_{suffix}"),
        )?);
    } else {
        // With no concrete CMAKE_BUILD_TYPE, configuration-specific flags do
        // not identify one build artifact. Instrumentation in any such entry
        // is therefore ambiguous rather than safe to ignore.
        for (_, suffix) in CMAKE_CONFIGURATION_SUFFIXES {
            for prefix in ["CMAKE_C_FLAGS_", "CMAKE_CXX_FLAGS_"] {
                let key = format!("{prefix}{suffix}");
                if let Some(value) = values.get(&key) {
                    if contains_asan_flag(value) || contains_coverage_flag(value) {
                        return Err(format!(
                            "selected Mooncake CMake cache has instrumentation flags in {key} but CMAKE_BUILD_TYPE is empty"
                        ));
                    }
                }
            }
        }
    }
    if !asan && flags.iter().any(|value| contains_asan_flag(value)) {
        return Err(
            "selected Mooncake CMake cache enables address/leak sanitizer flags while ENABLE_ASAN is OFF"
                .to_string(),
        );
    }
    Ok(InstrumentationConfig {
        asan,
        gcov: flags.iter().any(|value| contains_coverage_flag(value)),
    })
}

fn required_compiler_runtime_search_dir(
    search_dirs: &mut Vec<PathBuf>,
    candidates: &[&str],
    feature: &str,
) -> Result<(), String> {
    if candidates
        .iter()
        .any(|candidate| add_compiler_runtime_search_dir(search_dirs, candidate))
    {
        return Ok(());
    }
    Err(format!(
        "selected Mooncake CMake configuration requires {feature}, but its compiler runtime was not found"
    ))
}

fn instrumentation_link_libs(config: InstrumentationConfig) -> Vec<&'static str> {
    let mut libraries = Vec::new();
    if config.asan {
        libraries.push("asan");
    }
    if config.gcov {
        libraries.push("gcov");
    }
    libraries
}

fn push_existing_dir(search_dirs: &mut Vec<PathBuf>, dir: PathBuf) {
    if dir.is_dir() && !search_dirs.iter().any(|existing| existing == &dir) {
        search_dirs.push(dir);
    }
}

fn push_env_paths(search_dirs: &mut Vec<PathBuf>, name: &str) {
    if let Some(value) = env::var_os(name) {
        for dir in env::split_paths(&value) {
            push_existing_dir(search_dirs, dir);
        }
    }
}

fn push_cmake_prefix_lib_dirs(search_dirs: &mut Vec<PathBuf>, prefix: PathBuf) {
    push_existing_dir(search_dirs, prefix.join("lib"));
    push_existing_dir(search_dirs, prefix.join("lib64"));
}

fn push_cmake_prefix_paths(search_dirs: &mut Vec<PathBuf>, value: &str) {
    for prefix in value.split(';').filter(|prefix| !prefix.is_empty()) {
        push_cmake_prefix_lib_dirs(search_dirs, PathBuf::from(prefix));
    }
}

fn push_cmake_cache_library_dirs(search_dirs: &mut Vec<PathBuf>, build_dir: &PathBuf) {
    let cache_path = build_dir.join("CMakeCache.txt");
    let Ok(cache) = fs::read_to_string(cache_path) else {
        return;
    };

    for line in cache.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        if key == "CMAKE_PREFIX_PATH:PATH" {
            push_cmake_prefix_paths(search_dirs, value);
            continue;
        }

        if key.ends_with("_DIR:PATH") {
            let package_dir = PathBuf::from(value);
            if package_dir.parent().and_then(|dir| dir.file_name())
                == Some(std::ffi::OsStr::new("cmake"))
            {
                if let Some(lib_dir) = package_dir.ancestors().nth(2) {
                    push_existing_dir(search_dirs, lib_dir.to_path_buf());
                }
            }
            continue;
        }

        if key.ends_with("_LIBRARY:FILEPATH") || key.ends_with("_LIBRARIES:FILEPATH") {
            let library_path = PathBuf::from(value);
            if let Some(parent) = library_path.parent() {
                push_existing_dir(search_dirs, parent.to_path_buf());
            }
        }
    }
}

fn has_library(search_dirs: &[PathBuf], candidates: &[&str]) -> bool {
    search_dirs.iter().any(|dir| {
        candidates.iter().any(|candidate| {
            ["a", "so", "dylib"]
                .into_iter()
                .map(|ext| dir.join(format!("lib{candidate}.{ext}")))
                .any(|path| path.exists())
        })
    })
}

fn emit_link_searches(search_dirs: &[PathBuf]) {
    for dir in search_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
}

fn emit_runtime_rpaths(search_dirs: &[PathBuf]) {
    for dir in search_dirs {
        let dir_str = dir.display();
        // Use rustc-link-arg (not -tests) so that the rpath is also applied
        // to the lib-test binary produced by `cargo test` for #[cfg(test)]
        // modules inside src/lib.rs.
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir_str}");
    }
}

fn compiler_candidates() -> Vec<String> {
    let mut tools = Vec::new();

    for env_var in ["CC", "CXX"] {
        if let Ok(value) = env::var(env_var) {
            let tool = value.trim();
            if !tool.is_empty() && !tools.iter().any(|existing| existing == tool) {
                tools.push(tool.to_string());
            }
        }
    }

    for tool in ["gcc", "cc", "clang", "c++"] {
        if !tools.iter().any(|existing| existing == tool) {
            tools.push(tool.to_string());
        }
    }

    tools
}

fn compiler_runtime_library(file_name: &str) -> Option<PathBuf> {
    for tool in compiler_candidates() {
        let output = Command::new(&tool)
            .arg(format!("-print-file-name={file_name}"))
            .output()
            .ok()?;

        if !output.status.success() {
            continue;
        }

        let path = String::from_utf8(output.stdout).ok()?;
        let path = PathBuf::from(path.trim());
        if path.as_os_str().is_empty() || path == PathBuf::from(file_name) || !path.exists() {
            continue;
        }

        return Some(path);
    }

    None
}

fn add_compiler_runtime_search_dir(search_dirs: &mut Vec<PathBuf>, file_name: &str) -> bool {
    let Some(path) = compiler_runtime_library(file_name) else {
        return false;
    };

    if let Some(parent) = path.parent() {
        push_existing_dir(search_dirs, parent.to_path_buf());
        return true;
    }

    false
}

#[cfg(not(test))]
fn main() {
    // -----------------------------------------------------------------------
    // Library search path
    //
    // When built via CMake (WITH_STORE_RUST=ON) the CMakeLists.txt injects
    // MOONCAKE_STORE_LIB_DIR pointing at the directory that contains
    // libmooncake_store.a/.so.  When cargo is invoked standalone the caller
    // should set the variable manually or rely on the default convention of a
    // sibling `build/` directory produced by a top-level CMake configure.
    // -----------------------------------------------------------------------
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let configured_build_dir = nonempty_env_path("MOONCAKE_BUILD_DIR");
    let configured_store_lib_dir = nonempty_env_path("MOONCAKE_STORE_LIB_DIR");
    let selected_cache = selected_cmake_cache(
        &manifest_dir,
        configured_build_dir,
        configured_store_lib_dir.clone(),
    )
    .unwrap_or_else(|error| panic!("invalid selected Mooncake CMake cache: {error}"));
    println!(
        "cargo:rerun-if-changed={}",
        selected_cache.cache_path.display()
    );
    let instrumentation = configured_instrumentation(
        &fs::read_to_string(&selected_cache.cache_path).unwrap_or_else(|error| {
            panic!(
                "cannot read selected Mooncake CMake cache '{}': {error}",
                selected_cache.cache_path.display()
            )
        }),
    )
    .unwrap_or_else(|error| panic!("invalid selected Mooncake CMake configuration: {error}"));

    let build_dir = selected_cache.build_dir;
    let lib_path = configured_store_lib_dir.unwrap_or_else(|| build_dir.join("mooncake-store/src"));
    let lib_dir = lib_path.display();

    println!("cargo:rustc-link-search=native={lib_dir}");

    // mooncake_store depends on libasio.so (shared) built in mooncake-common.
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("mooncake-common").display()
    );
    // mooncake_common static library lives in the src/ subdirectory.
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("mooncake-common/src").display()
    );

    // transfer_engine is built in a sibling directory.
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir.join("mooncake-transfer-engine/src").display()
    );

    // common/base library (contains mooncake::Status etc.)
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir
            .join("mooncake-transfer-engine/src/common/base")
            .display()
    );

    // CUDA runtime libraries (needed by transfer_engine RDMA transport).
    let cuda_home = env::var("CUDA_HOME")
        .or_else(|_| env::var("CUDA_PATH"))
        .unwrap_or_else(|_| "/usr/local/cuda".to_string());
    println!(
        "cargo:rustc-link-search=native={}/targets/x86_64-linux/lib",
        cuda_home
    );

    // cachelib_memory_allocator is a static library built alongside mooncake_store.
    println!(
        "cargo:rustc-link-search=native={}",
        build_dir
            .join("mooncake-store/src/cachelib_memory_allocator")
            .display()
    );

    println!("cargo:rustc-link-lib=mooncake_store");

    // Dependencies of mooncake_store that must be satisfied at link time.
    // The list mirrors what mooncake-store/src/CMakeLists.txt links against.
    println!("cargo:rustc-link-lib=transfer_engine");
    println!("cargo:rustc-link-lib=mooncake_common"); // Environ::Get() and other common utilities
    println!("cargo:rustc-link-lib=base"); // mooncake::Status etc.
    println!("cargo:rustc-link-lib=asio"); // shared library built by mooncake-common
    println!("cargo:rustc-link-lib=jsoncpp"); // transfer_engine dependency
    println!("cargo:rustc-link-lib=cachelib_memory_allocator"); // static
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=glog");
    println!("cargo:rustc-link-lib=gflags");
    println!("cargo:rustc-link-lib=numa"); // NUMA binding
    println!("cargo:rustc-link-lib=curl"); // HTTP metadata plugin
    println!("cargo:rustc-link-lib=ibverbs"); // RDMA transport
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=xxhash");

    // -----------------------------------------------------------------------
    // Header path for bindgen
    // -----------------------------------------------------------------------
    let mut search_dirs = Vec::new();
    push_existing_dir(&mut search_dirs, lib_path);
    push_cmake_cache_library_dirs(&mut search_dirs, &build_dir);
    for dir in [
        build_dir.join("mooncake-store/src"),
        build_dir.join("mooncake-store/src/cachelib_memory_allocator"),
        build_dir.join("mooncake-transfer-engine/src"),
        build_dir.join("mooncake-transfer-engine/src/common/base"),
        build_dir.join("mooncake-asio"),
        build_dir.join("mooncake-common"),
        build_dir.join("mooncake-common/etcd"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        PathBuf::from("/lib/x86_64-linux-gnu"),
    ] {
        push_existing_dir(&mut search_dirs, dir);
    }

    if let Ok(cmake_prefix_path) = env::var("CMAKE_PREFIX_PATH") {
        push_cmake_prefix_paths(&mut search_dirs, &cmake_prefix_path);
    }

    push_env_paths(&mut search_dirs, "LD_LIBRARY_PATH");
    push_env_paths(&mut search_dirs, "LIBRARY_PATH");

    if instrumentation.asan {
        required_compiler_runtime_search_dir(
            &mut search_dirs,
            &["libasan.so", "libasan.a"],
            "address sanitizer runtime",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    }
    if instrumentation.gcov {
        required_compiler_runtime_search_dir(
            &mut search_dirs,
            &["libgcov.a", "libgcov.so"],
            "gcov runtime",
        )
        .unwrap_or_else(|error| panic!("{error}"));
    }

    emit_link_searches(&search_dirs);
    emit_runtime_rpaths(&search_dirs);

    for library in instrumentation_link_libs(instrumentation) {
        println!("cargo:rustc-link-lib={library}");
    }

    for library in [
        "mooncake_store",
        "cachelib_memory_allocator",
        "transfer_engine",
        "base",
        "asio",
        "stdc++",
        "glog",
        "gflags",
        "pthread",
        "xxhash",
        "numa",
        "ibverbs",
        "jsoncpp",
        "zstd",
        "m",
    ] {
        println!("cargo:rustc-link-lib={library}");
    }

    for (link_name, candidates) in [
        ("etcd_wrapper", &["etcd_wrapper"] as &[&str]),
        ("hiredis", &["hiredis"]),
        ("curl", &["curl"]),
        ("cuda", &["cuda"]),
        ("cudart", &["cudart"]),
        ("uring", &["uring"]),
    ] {
        if has_library(&search_dirs, candidates) {
            println!("cargo:rustc-link-lib={link_name}");
        }
    }

    let include_dir =
        env::var("MOONCAKE_STORE_INCLUDE_DIR").unwrap_or_else(|_| "../include".to_string());

    let header = format!("{include_dir}/store_c.h");

    println!("cargo:rerun-if-changed={header}");
    println!("cargo:rerun-if-env-changed=MOONCAKE_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=MOONCAKE_STORE_LIB_DIR");
    println!("cargo:rerun-if-env-changed=MOONCAKE_STORE_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=LD_LIBRARY_PATH");
    println!("cargo:rerun-if-env-changed=LIBRARY_PATH");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=CXX");

    let bindings = bindgen::Builder::default()
        .header(&header)
        .allowlist_function("mooncake_store_.*")
        .allowlist_type("mooncake_.*")
        .generate()
        .expect("Unable to generate Mooncake Store bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write Mooncake Store bindings");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cache(build_type: &str, asan: &str, global_flags: &str, config_flags: &str) -> String {
        format!(
            "ENABLE_ASAN:BOOL={asan}\n\
             CMAKE_BUILD_TYPE:STRING={build_type}\n\
             CMAKE_C_FLAGS:STRING={global_flags}\n\
             CMAKE_CXX_FLAGS:STRING={global_flags}\n\
             CMAKE_C_FLAGS_RELEASE:STRING={config_flags}\n\
             CMAKE_CXX_FLAGS_RELEASE:STRING={config_flags}\n"
        )
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "mooncake-build-rs-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temporary directory");
        path
    }

    fn write_cache(root: &Path, contents: &str) {
        fs::write(root.join("CMakeCache.txt"), contents).expect("write CMake cache");
    }

    #[test]
    fn off_cache_emits_no_special_runtime_link_directives() {
        let config =
            configured_instrumentation(&cache("Release", "OFF", "", "")).expect("OFF cache parses");
        assert_eq!(
            config,
            InstrumentationConfig {
                asan: false,
                gcov: false,
            }
        );
        let root = temporary_directory("host-runtime-present");
        fs::write(root.join("libasan.so"), b"fixture").expect("write fake asan runtime");
        fs::write(root.join("libgcov.a"), b"fixture").expect("write fake gcov runtime");
        assert!(has_library(&[root.clone()], &["asan"]));
        assert!(has_library(&[root.clone()], &["gcov"]));
        // Even with sanitizer runtimes discoverable, directives are derived
        // only from the selected cache-backed configuration.
        assert!(instrumentation_link_libs(config).is_empty());
        fs::remove_dir_all(root).expect("remove temporary directory");
    }

    #[test]
    fn on_cache_emits_asan_only() {
        let config =
            configured_instrumentation(&cache("Release", "ON", "", "")).expect("ON cache parses");
        assert_eq!(instrumentation_link_libs(config), vec!["asan"]);
    }

    #[test]
    fn configuration_specific_coverage_emits_gcov() {
        for (build_type, suffix) in CMAKE_CONFIGURATION_SUFFIXES {
            let config = configured_instrumentation(&format!(
                "ENABLE_ASAN:BOOL=OFF\n\
                 CMAKE_BUILD_TYPE:STRING={build_type}\n\
                 CMAKE_C_FLAGS:STRING=\n\
                 CMAKE_CXX_FLAGS:STRING=\n\
                 CMAKE_C_FLAGS_{suffix}:STRING=-O3 -fprofile-arcs -ftest-coverage\n\
                 CMAKE_CXX_FLAGS_{suffix}:STRING=\n"
            ))
            .expect("coverage cache parses");
            assert_eq!(instrumentation_link_libs(config), vec!["gcov"]);
        }
    }

    #[test]
    fn empty_build_type_rejects_configuration_specific_instrumentation() {
        for flag in ["-fprofile-arcs", "-fsanitize=address"] {
            let contents = format!(
                "ENABLE_ASAN:BOOL=OFF\n\
                 CMAKE_BUILD_TYPE:STRING=\n\
                 CMAKE_C_FLAGS:STRING=\n\
                 CMAKE_CXX_FLAGS:STRING=\n\
                 CMAKE_CXX_FLAGS_RELEASE:STRING={flag}\n"
            );
            assert!(configured_instrumentation(&contents).is_err());
        }

        let config = configured_instrumentation(
            "ENABLE_ASAN:BOOL=OFF\n\
             CMAKE_BUILD_TYPE:STRING=Release\n\
             CMAKE_C_FLAGS:STRING=\n\
             CMAKE_CXX_FLAGS:STRING=\n\
             CMAKE_C_FLAGS_RELEASE:STRING=\n\
             CMAKE_CXX_FLAGS_RELEASE:STRING=\n\
             CMAKE_CXX_FLAGS_DEBUG:STRING=-fprofile-arcs\n",
        )
        .expect("nonselected Debug instrumentation must not affect a Release build");
        assert!(instrumentation_link_libs(config).is_empty());
    }

    #[test]
    fn malformed_or_duplicate_relevant_entries_fail_closed() {
        let wrong_type = "ENABLE_ASAN:STRING=OFF\n";
        assert!(configured_instrumentation(wrong_type).is_err());

        let bad_bool = "ENABLE_ASAN:BOOL=not-a-bool\n\
                        CMAKE_BUILD_TYPE:STRING=Release\n\
                        CMAKE_C_FLAGS:STRING=\n\
                        CMAKE_CXX_FLAGS:STRING=\n\
                        CMAKE_C_FLAGS_RELEASE:STRING=\n\
                        CMAKE_CXX_FLAGS_RELEASE:STRING=\n";
        assert!(configured_instrumentation(bad_bool).is_err());

        let duplicate = format!("{}ENABLE_ASAN:BOOL=OFF\n", cache("Release", "OFF", "", ""));
        assert!(configured_instrumentation(&duplicate).is_err());
    }

    #[test]
    fn explicit_and_derived_cache_roots_must_match() {
        let root = temporary_directory("mismatch");
        let explicit = root.join("explicit");
        let derived = root.join("derived");
        fs::create_dir_all(derived.join("mooncake-store/src")).expect("create derived store dir");
        fs::create_dir_all(&explicit).expect("create explicit build dir");
        write_cache(&explicit, &cache("Release", "OFF", "", ""));
        let error = selected_cmake_cache(
            &root,
            Some(explicit),
            Some(derived.join("mooncake-store/src")),
        )
        .expect_err("mismatched roots must fail");
        assert!(error.contains("does not match"), "{}", error);
        fs::remove_dir_all(root).expect("remove temporary directory");
    }

    #[test]
    fn selected_cache_must_exist() {
        let root = temporary_directory("missing-cache");
        let error = selected_cmake_cache(&root, Some(root.clone()), None)
            .expect_err("missing selected cache must fail");
        assert!(error.contains("is unavailable"), "{}", error);
        fs::remove_dir_all(root).expect("remove temporary directory");
    }
}
