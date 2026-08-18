use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use troupe_diagnostics_runtime::{
    archive::{
        constants as archive_constants, layout as archive_layout, lease as archive_lease,
        probe as archive_probe,
    },
    query::{
        aggregate as query_aggregate, events as query_events, filter as query_filter,
        pagination as query_pagination, reader as query_reader, snapshot as query_snapshot,
        status as query_status,
    },
    registry::{
        codec as registry_codec, discover as registry_discover, model as registry_model,
        process_identity as registry_process_identity, publish as registry_publish,
        revalidate as registry_revalidate,
    },
    server::{
        assembly as server_assembly, assets as server_assets, dump as server_dump,
        error as server_error, identity as server_identity, query as server_query,
        routes as server_routes, runtime as server_runtime, service as server_service,
        sse::{
            cursor as server_sse_cursor, frame as server_sse_frame, replay as server_sse_replay,
            subscriber as server_sse_subscriber,
        },
    },
    store::{
        admission as store_admission, batch as store_batch, connection as store_connection,
        key as store_key, progress as store_progress,
        projector::{
            counters as store_projector_counters, messages as store_projector_messages,
            plans as store_projector_plans, snapshot as store_projector_snapshot,
            spans as store_projector_spans, usage as store_projector_usage,
        },
        quota as store_quota, schema as store_schema, watermark as store_watermark,
        writer as store_writer,
    },
};

const LEAF_SLOTS: &[&str] = &[
    "archive/constants.rs",
    "archive/layout.rs",
    "archive/lease.rs",
    "archive/probe.rs",
    "query/aggregate.rs",
    "query/events.rs",
    "query/filter.rs",
    "query/pagination.rs",
    "query/reader.rs",
    "query/snapshot.rs",
    "query/status.rs",
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
        "pub mod constants;\npub mod layout;\npub mod lease;\npub mod probe;\n",
    ),
    (
        "query/mod.rs",
        "pub mod aggregate;\npub mod events;\npub mod filter;\npub mod pagination;\npub mod reader;\npub mod snapshot;\npub mod status;\n",
    ),
    (
        "registry/mod.rs",
        "pub mod codec;\npub mod discover;\npub mod model;\npub mod process_identity;\npub mod publish;\npub mod revalidate;\n",
    ),
    (
        "server/mod.rs",
        "pub mod assembly;\npub mod assets;\npub mod dump;\npub mod error;\npub mod identity;\npub mod query;\npub mod routes;\npub mod runtime;\npub mod service;\npub mod sse;\n",
    ),
    (
        "server/sse/mod.rs",
        "pub mod cursor;\npub mod frame;\npub mod replay;\npub mod subscriber;\n",
    ),
    (
        "store/mod.rs",
        "pub mod admission;\npub mod batch;\npub mod connection;\npub mod key;\npub mod progress;\npub mod projector;\npub mod quota;\npub mod schema;\npub mod watermark;\npub mod writer;\n",
    ),
    (
        "store/projector/mod.rs",
        "pub mod counters;\npub mod messages;\npub mod plans;\npub mod snapshot;\npub mod spans;\npub mod usage;\n",
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
