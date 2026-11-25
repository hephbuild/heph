//! I4: a `--json` field costs what its tier declares, and nothing is computed
//! for a field that is not selected.
//!
//! Table-driven over `QueryRow::FIELDS`, the only record with fields that
//! cost anything (every other record's fields are `free`, which
//! `json_contract::default_fields_are_free` and the field snapshot pin): each
//! field is selected alone, on a fresh engine, and the provider's spec gets,
//! the driver's parses and its runs are counted against the tier.
//!
//! | Tier | spec gets | def parses | runs |
//! |---|---|---|---|
//! | free | as `--fields addr` | 0 | 0 |
//! | resolved | — | > 0 | 0 |
//! | built | — | — | > 0 |

use futures::StreamExt;
use heph::commands::query::{QueryRow, push_json_rows, select};
use heph::engine::driver_managed::{ManagedDriver, ManagedRunRequest, ManagedRunResponse};
use heph::engine::fault_provider::{FaultProvider, Faults, GetLog};
use heph::engine::{Discovery, Gaps};
use heph::htmatcher::Matcher;
use heph::htpkg::PkgBuf;
use heph::pluginexec;
use heph::pluginstatictarget::Target;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The bash driver, counting the defs it parses and the targets it runs.
struct Counted {
    bash: pluginexec::Driver,
    parses: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ManagedDriver for Counted {
    fn config(
        &self,
        _req: hplugin::driver::ConfigRequest,
    ) -> anyhow::Result<hplugin::driver::ConfigResponse> {
        Ok(hplugin::driver::ConfigResponse {
            name: "counted".to_string(),
        })
    }
    fn schema(&self) -> hplugin::driver::DriverSchema {
        ManagedDriver::schema(&self.bash)
    }
    async fn parse(
        &self,
        req: hplugin::driver::ParseRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ParseResponse> {
        self.parses.fetch_add(1, Ordering::SeqCst);
        ManagedDriver::parse(&self.bash, req, ctoken).await
    }
    async fn apply_transitive(
        &self,
        req: hplugin::driver::ApplyTransitiveRequest,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<hplugin::driver::ApplyTransitiveResponse> {
        ManagedDriver::apply_transitive(&self.bash, req, ctoken).await
    }
    async fn run<'a, 'io>(
        &self,
        req: ManagedRunRequest<'a, 'io>,
        ctoken: &(dyn hcore::hasync::Cancellable + Send + Sync),
    ) -> anyhow::Result<ManagedRunResponse> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ManagedDriver::run(&self.bash, req, ctoken).await
    }
}

fn target(addr: &str, deps: &[&str]) -> Target {
    Target {
        addr: addr.to_string(),
        driver: "counted".to_string(),
        run: Some("echo ok > $OUT".to_string()),
        out: HashMap::from([(String::new(), vec!["out.txt".to_string()])]),
        deps: HashMap::from([(
            String::new(),
            deps.iter().map(|d| (*d).to_string()).collect(),
        )]),
        ..Default::default()
    }
}

/// What one `query --json --fields <field>` cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cost {
    gets: usize,
    parses: usize,
    runs: usize,
}

/// `heph query //p --json --fields <field>` on a fresh engine: its rows and
/// what it cost.
async fn query_cost(field: &str) -> anyhow::Result<(Vec<serde_json::Value>, Cost)> {
    let provider = FaultProvider::new(
        vec![target("//p:dep", &[]), target("//p:top", &["//p:dep"])],
        Faults::default(),
    )?;
    let gets: Arc<GetLog> = provider.gets();
    let parses = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let ws = htestkit::WorkspaceBuilder::new()?
        .with_provider(move |_| Box::new(provider))
        .with_managed_driver(Box::new(Counted {
            bash: pluginexec::Driver::new_bash(),
            parses: Arc::clone(&parses),
            runs: Arc::clone(&runs),
        }))
        .build()?;

    let out = output::JsonArgs::<QueryRow>::try_parse_from(["--fields", field])?;
    let m = Matcher::Package(PkgBuf::from("p"));
    let gaps = Gaps::new("//p".to_string());
    let rs = ws.engine.new_state();
    let addrs = select(
        Arc::clone(&ws.engine),
        rs.clone(),
        &m,
        Discovery::keep_going_unless(false, &gaps),
        false,
    )
    .boxed();
    let mut buf = Vec::new();
    let mut list = out
        .printer_to(&mut buf, output::Style::Compact)
        .start_list()?;
    let flow = push_json_rows(&ws.engine, &rs, &out, false, addrs, &mut list).await?;
    anyhow::ensure!(flow == output::Flow::Continue, "the list stopped");
    anyhow::ensure!(gaps.is_empty(), "the walk skipped something");
    list.finish(&output::never_cancelled)?;
    drop(rs);

    let rows: Vec<serde_json::Value> = serde_json::from_slice(&buf)?;
    let cost = Cost {
        gets: gets.addrs().len(),
        parses: parses.load(Ordering::SeqCst),
        runs: runs.load(Ordering::SeqCst),
    };
    Ok((rows, cost))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_field_costs_what_it_declares() {
    use output::Record as _;
    let (_, base) = query_cost("addr").await.expect("query --fields addr");
    assert_eq!(
        (base.parses, base.runs),
        (0, 0),
        "plain query parses and runs nothing: {base:?}"
    );

    for f in QueryRow::FIELDS {
        let (rows, cost) = query_cost(f.name)
            .await
            .unwrap_or_else(|e| panic!("query --fields {}: {e:#}", f.name));
        assert_eq!(rows.len(), 2, "{}: {rows:?}", f.name);
        // The field was computed: non-null on each row.
        for row in &rows {
            let obj = row.as_object().expect("a row is an object");
            assert_eq!(obj.len(), 1, "{}: only the selected key: {row}", f.name);
            assert!(!obj[f.name].is_null(), "{}: not computed: {row}", f.name);
        }
        match f.tier {
            output::Tier::Free => assert_eq!(cost, base, "{} is free", f.name),
            output::Tier::Resolved => {
                assert!(cost.parses > 0, "{} resolves a def: {cost:?}", f.name);
                assert_eq!(cost.runs, 0, "{} builds nothing: {cost:?}", f.name);
            }
            output::Tier::Built => {
                assert!(cost.runs > 0, "{} builds dependencies: {cost:?}", f.name);
            }
        }
    }
}
