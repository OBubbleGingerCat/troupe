use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use troupe_diagnostics_perfetto::{atomic_file, collect, dump, identity, project, schema, tracks};

const MODULES: &[&str] = &[
    "atomic_file",
    "collect",
    "dump",
    "identity",
    "project",
    "schema",
    "tracks",
];

fn rust_sources(root: &Path, directory: &Path, output: &mut BTreeSet<String>) {
    for entry in fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            rust_sources(root, &path, output);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.insert(
                path.strip_prefix(root)
                    .expect("source stays below crate root")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

#[test]
fn perfetto_module_slots_are_exact_and_workspace_visible() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_root = crate_root.join("src");
    let declarations = MODULES
        .iter()
        .map(|module| format!("pub mod {module};"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        fs::read_to_string(source_root.join("lib.rs")).expect("read crate root"),
        format!("#![allow(dead_code)]\n\n{declarations}\n")
    );

    let expected = std::iter::once("src/lib.rs".to_owned())
        .chain(MODULES.iter().map(|module| format!("src/{module}.rs")))
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    rust_sources(&crate_root, &source_root, &mut actual);
    assert_eq!(actual, expected);
}
