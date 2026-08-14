use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use troupe_diagnostics_runtime::{archive, query, registry, server, store};

const LEAF_SLOTS: &[&str] = &[
    "archive/constants.rs",
    "archive/layout.rs",
    "archive/lease.rs",
    "archive/probe.rs",
    "query/aggregate.rs",
    "query/archive_views.rs",
    "query/events.rs",
    "query/filter.rs",
    "query/pagination.rs",
    "query/reader.rs",
    "query/snapshot.rs",
    "query/status.rs",
    "query/views.rs",
    "registry/codec.rs",
    "registry/discover.rs",
    "registry/model.rs",
    "registry/process_identity.rs",
    "registry/publish.rs",
    "registry/revalidate.rs",
    "server/assembly.rs",
    "server/assets.rs",
    "server/dump.rs",
    "server/error.rs",
    "server/identity.rs",
    "server/query.rs",
    "server/routes.rs",
    "server/runtime.rs",
    "server/service.rs",
    "server/sse/cursor.rs",
    "server/sse/frame.rs",
    "server/sse/replay.rs",
    "server/sse/subscriber.rs",
    "server/views.rs",
    "store/admission.rs",
    "store/batch.rs",
    "store/connection.rs",
    "store/key.rs",
    "store/progress.rs",
    "store/projector/counters.rs",
    "store/projector/messages.rs",
    "store/projector/plans.rs",
    "store/projector/snapshot.rs",
    "store/projector/spans.rs",
    "store/projector/usage.rs",
    "store/quota.rs",
    "store/schema.rs",
    "store/view_records.rs",
    "store/watermark.rs",
    "store/writer.rs",
];

const DECLARATIONS: &[(&str, &str)] = &[
    (
        "lib.rs",
        "#![allow(dead_code)]\n\npub mod archive;\npub mod query;\npub mod registry;\npub mod server;\npub mod store;\n",
    ),
    (
        "archive/mod.rs",
        "mod constants;\nmod layout;\nmod lease;\nmod probe;\n",
    ),
    (
        "query/mod.rs",
        "mod aggregate;\nmod archive_views;\nmod events;\nmod filter;\nmod pagination;\nmod reader;\nmod snapshot;\nmod status;\nmod views;\n",
    ),
    (
        "registry/mod.rs",
        "mod codec;\nmod discover;\nmod model;\nmod process_identity;\nmod publish;\nmod revalidate;\n",
    ),
    (
        "server/mod.rs",
        "mod assembly;\nmod assets;\nmod dump;\nmod error;\nmod identity;\nmod query;\nmod routes;\nmod runtime;\nmod service;\nmod sse;\nmod views;\n",
    ),
    (
        "server/sse/mod.rs",
        "mod cursor;\nmod frame;\nmod replay;\nmod subscriber;\n",
    ),
    (
        "store/mod.rs",
        "mod admission;\nmod batch;\nmod connection;\nmod key;\nmod progress;\nmod projector;\nmod quota;\nmod schema;\nmod view_records;\nmod watermark;\nmod writer;\n",
    ),
    (
        "store/projector/mod.rs",
        "mod counters;\nmod messages;\nmod plans;\nmod snapshot;\nmod spans;\nmod usage;\n",
    ),
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
fn runtime_module_slots_are_exact_and_workspace_visible() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_root = crate_root.join("src");

    for (path, expected) in DECLARATIONS {
        assert_eq!(
            fs::read_to_string(source_root.join(path)).expect("read declaration module"),
            *expected,
            "unexpected declarations in {path}"
        );
    }

    let expected = DECLARATIONS
        .iter()
        .map(|(path, _)| format!("src/{path}"))
        .chain(LEAF_SLOTS.iter().map(|path| format!("src/{path}")))
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    rust_sources(&crate_root, &source_root, &mut actual);
    assert_eq!(actual, expected);
}
