use std::{
    borrow::Cow,
    cmp,
    collections::{
        BTreeMap,
        BTreeSet,
    },
    convert::{
        TryFrom,
        TryInto,
    },
    marker::PhantomData,
    ops::{
        Bound,
        Deref,
    },
    sync::{
        atomic::{
            AtomicUsize,
            Ordering,
        },
        Arc,
        LazyLock,
        OnceLock,
    },
    time::{
        Duration,
        Instant,
    },
};

use anyhow::{
    Context,
    Error,
};
use cmd_util::env::env_config;
use common::{
    bootstrap_model::{
        index::{
            database_index::IndexedFields,
            IndexMetadata,
            TabletIndexMetadata,
            INDEX_TABLE,
        },
        tables::{
            TableMetadata,
            TableState,
            TABLES_TABLE,
        },
    },
    document::{
        CreationTime,
        DocumentUpdate,
        InternalId,
        ParsedDocument,
        ResolvedDocument,
    },
    interval::Interval,
    knobs::DEFAULT_DOCUMENTS_PAGE_SIZE,
    pause::PauseClient,
    persistence::{
        new_idle_repeatable_ts,
        ConflictStrategy,
        DocumentStream,
        LatestDocumentStream,
        Persistence,
        PersistenceGlobalKey,
        PersistenceReader,
        PersistenceSnapshot,
        RepeatablePersistence,
        RetentionValidator,
        TimestampRange,
    },
    query::Order,
    runtime::{
        new_rate_limiter,
        RateLimiter,
        Runtime,
        RuntimeInstant,
    },
    sync::split_rw_lock::{
        new_split_rw_lock,
        Reader,
    },
    types::{
        GenericIndexName,
        IndexId,
        IndexName,
        PersistenceVersion,
        RepeatableTimestamp,
        TableName,
        TabletIndexName,
        Timestamp,
    },
    value::{
        ConvexObject,
        GenericDocumentId,
        ResolvedDocumentId,
        TableId,
        TableIdAndTableNumber,
        TableIdentifier,
        TableMapping,
        VirtualTableMapping,
    },
};
use errors::{
    ErrorMetadata,
    ErrorMetadataAnyhowExt,
};
use events::usage::UsageEventLogger;
use futures::{
    future::BoxFuture,
    pin_mut,
    Future,
    Stream,
    StreamExt,
    TryStreamExt,
};
use governor::Quota;
use imbl::OrdMap;
use indexing::{
    backend_in_memory_indexes::{
        BackendInMemoryIndexes,
        DatabaseIndexSnapshot,
    },
    index_registry::IndexRegistry,
};
use itertools::Itertools;
use keybroker::Identity;
use minitrace::{
    collector::SpanContext,
    full_name,
    future::FutureExt,
    Span,
};
use pb::funrun::BootstrapMetadata as BootstrapMetadataProto;
use search::{
    query::RevisionWithKeys,
    SearchIndexManager,
    SearchIndexManagerState,
    Searcher,
};
use storage::Storage;
use sync_types::backoff::Backoff;
use usage_tracking::{
    FunctionUsageStats,
    FunctionUsageTracker,
    UsageCounter,
};
use value::{
    heap_size::HeapSize,
    id_v6::DocumentIdV6,
    Size,
    TableNumber,
};
use vector::{
    PublicVectorSearchQueryResult,
    VectorIndexManager,
    VectorSearch,
};

use crate::{
    bootstrap_model::{
        table::{
            NUM_RESERVED_LEGACY_TABLE_NUMBERS,
            NUM_RESERVED_SYSTEM_TABLE_NUMBERS,
        },
        virtual_tables::{
            types::VirtualTableMetadata,
            VIRTUAL_TABLES_TABLE,
        },
    },
    committer::{
        Committer,
        CommitterClient,
    },
    defaults::{
        bootstrap_system_tables,
        SystemIndex,
        DEFAULT_BOOTSTRAP_TABLE_NUMBERS,
    },
    metrics::{
        self,
        load_indexes_into_memory_timer,
        load_virtual_table_metadata_timer,
        vector::vector_search_with_retries_timer,
        verify_invariants_timer,
    },
    retention::LeaderRetentionManager,
    search_and_vector_bootstrap::SearchAndVectorIndexBootstrapWorker,
    snapshot_manager::{
        Snapshot,
        SnapshotManager,
        TableSummaries,
    },
    stack_traces::StackTrace,
    subscription::{
        Subscription,
        SubscriptionsClient,
        SubscriptionsWorker,
    },
    table_registry::TableRegistry,
    table_summary::{
        self,
        TableSummarySnapshot,
    },
    token::Token,
    transaction_id_generator::TransactionIdGenerator,
    transaction_index::{
        SearchIndexManagerSnapshot,
        TransactionIndex,
    },
    write_log::{
        new_write_log,
        LogReader,
        WriteSource,
    },
    FollowerRetentionManager,
    TableIterator,
    Transaction,
    VirtualSystemMapping,
};

/// Controls the number of read set backtraces to show when debugging
/// OCC errors. Collecting stack traces is expensive and should only
/// be used in development.
///
/// Must be used in tandem with `READ_SET_CAPTURE_BACKTRACES`.
static NUM_READ_SET_STACKS: LazyLock<usize> =
    LazyLock::new(|| env_config("NUM_READ_SET_STACKS", 1));
const INITIAL_OCC_BACKOFF: Duration = Duration::from_millis(10);
const MAX_OCC_BACKOFF: Duration = Duration::from_secs(2);
pub const MAX_OCC_FAILURES: u32 = 3;

/// In memory vector changes are asynchronously backfilled on startup. Attempts
/// to query before backfill is finished will result in failure, so we need to
/// retry. Vector search is latency tolerant because it's only run in actions,
/// so we can retry for a while before we have to fail.
const INITIAL_VECTOR_BACKOFF: Duration = Duration::from_millis(150);
const MAX_VECTOR_BACKOFF: Duration = Duration::from_millis(2500);
// 150 * 2^5 ~= 5000 or 5 seconds total.
const MAX_VECTOR_ATTEMPTS: u32 = 5;

/// Public entry point for interacting with the database.
///
/// This structure is cheap to clone and can be shared throughout the
/// application. Internally, it only has read-only access to the database
/// metadata, document store, and index manager.
/// Beginning a transaction chooses a timestamp and procures a snapshot of the
/// DocumentStore and DatabaseIndex data structures, so operations on the
/// [Transaction] don't even need to acquire [Database]'s read-lock.
///
/// Then, the [Committer], accessed via the [CommitterClient], has exclusive
/// access to mutate the database state. Calling [Database::commit] sends a
/// message to the [Committer] task, which then applies each transaction
/// serially.
///
/// See the diagram in `database/README.md` for more details.
#[derive(Clone)]
pub struct Database<RT: Runtime> {
    committer: CommitterClient<RT>,
    subscriptions: SubscriptionsClient<RT>,
    log: LogReader,
    snapshot_manager: Reader<SnapshotManager>,
    pub(crate) runtime: RT,
    reader: Arc<dyn PersistenceReader>,
    write_commits_since_load: Arc<AtomicUsize>,
    retention_manager: LeaderRetentionManager<RT>,
    pub searcher: Arc<dyn Searcher>,
    pub search_storage: Arc<OnceLock<Arc<dyn Storage>>>,
    usage_counter: UsageCounter,
    virtual_system_mapping: VirtualSystemMapping,
    pub bootstrap_metadata: BootstrapMetadata,
}

#[derive(Clone)]
pub struct DatabaseSnapshot {
    ts: RepeatableTimestamp,
    pub bootstrap_metadata: BootstrapMetadata,
    pub snapshot: Snapshot,
    pub persistence_snapshot: PersistenceSnapshot,

    summaries_num_rows: usize,

    // To read lots of data at the snapshot, sometimes you need
    // to look at current data and walk backwards.
    // Use the `table_iterator` method to do that -- don't access these
    // fields directly.
    pub persistence_reader: Arc<dyn PersistenceReader>,
    pub retention_validator: Arc<dyn RetentionValidator>,
}

#[derive(PartialEq, Eq, Debug)]
pub struct DocumentDeltas {
    /// Document deltas returned in increasing (ts, table_id, id) order.
    /// We use ResolvedDocument here rather than DeveloperDocument
    /// because streaming export always uses string IDs
    pub deltas: Vec<(Timestamp, DocumentIdV6, TableName, Option<ResolvedDocument>)>,
    /// Exclusive cursor timestamp to pass in to the next call to
    /// document_deltas.
    pub cursor: Timestamp,
    /// Continue calling document_deltas while has_more is true.
    pub has_more: bool,
}

#[derive(PartialEq, Eq, Debug)]
pub struct SnapshotPage {
    pub documents: Vec<(Timestamp, TableName, ResolvedDocument)>,
    pub snapshot: Timestamp,
    pub cursor: Option<DocumentIdV6>,
    pub has_more: bool,
}

#[cfg_attr(
    any(test, feature = "testing"),
    derive(proptest_derive::Arbitrary, Debug, PartialEq,)
)]
#[derive(Clone)]
pub struct BootstrapMetadata {
    pub tables_by_id: IndexId,
    pub index_by_id: IndexId,
    pub tables_table_id: TableId,
    pub index_table_id: TableId,
}

impl From<BootstrapMetadata> for BootstrapMetadataProto {
    fn from(
        BootstrapMetadata {
            tables_by_id,
            index_by_id,
            tables_table_id,
            index_table_id,
        }: BootstrapMetadata,
    ) -> Self {
        Self {
            tables_by_id: Some(tables_by_id.into()),
            index_by_id: Some(index_by_id.into()),
            tables_table_id: Some(tables_table_id.0.into()),
            index_table_id: Some(index_table_id.0.into()),
        }
    }
}

impl TryFrom<BootstrapMetadataProto> for BootstrapMetadata {
    type Error = anyhow::Error;

    fn try_from(
        BootstrapMetadataProto {
            tables_by_id,
            index_by_id,
            tables_table_id,
            index_table_id,
        }: BootstrapMetadataProto,
    ) -> anyhow::Result<Self> {
        let tables_by_id = tables_by_id.context("Missing tables_by_id")?.try_into()?;
        let index_by_id = index_by_id.context("Missing index_by_id")?.try_into()?;
        let tables_table_id = TableId(
            tables_table_id
                .context("Missing tables_table_id")?
                .try_into()?,
        );
        let index_table_id = TableId(
            index_table_id
                .context("Missing index_table_id")?
                .try_into()?,
        );
        Ok(BootstrapMetadata {
            tables_by_id,
            index_by_id,
            tables_table_id,
            index_table_id,
        })
    }
}

impl DatabaseSnapshot {
    pub async fn max_ts(reader: &dyn PersistenceReader) -> anyhow::Result<Timestamp> {
        reader
            .max_ts()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no documents -- cannot load uninitialized database"))
    }

    pub async fn load_raw_table_documents(
        persistence_snapshot: &PersistenceSnapshot,
        index_id: IndexId,
        table_id: TableId,
    ) -> anyhow::Result<BTreeMap<ResolvedDocumentId, (Timestamp, ResolvedDocument)>> {
        persistence_snapshot
            .index_scan(index_id, table_id, &Interval::all(), Order::Asc, usize::MAX)
            .map_ok(|(_, ts, doc)| (*doc.id(), (ts, doc)))
            .try_collect()
            .await
    }

    async fn load_table_documents<D: TryFrom<ConvexObject, Error = anyhow::Error>>(
        persistence_snapshot: &PersistenceSnapshot,
        index_id: IndexId,
        table_id: TableId,
    ) -> anyhow::Result<Vec<ParsedDocument<D>>> {
        Self::load_raw_table_documents(persistence_snapshot, index_id, table_id)
            .await?
            .into_values()
            .map(|(_, doc)| doc.try_into())
            .try_collect()
    }

    pub fn table_mapping_and_states(
        table_documents: Vec<ParsedDocument<TableMetadata>>,
    ) -> (TableMapping, OrdMap<TableId, TableState>) {
        let mut table_mapping = TableMapping::new();
        let mut table_states = OrdMap::new();
        for table_doc in table_documents {
            let table_id = TableId(table_doc.id().internal_id());
            table_states.insert(table_id, table_doc.state);
            let table_number = table_doc.number;
            match table_doc.state {
                TableState::Active => {
                    table_mapping.insert(table_id, table_number, table_doc.into_value().name)
                },
                TableState::Hidden => {
                    table_mapping.insert_tablet(table_id, table_number, table_doc.into_value().name)
                },
                TableState::Deleting => {},
            }
        }
        (table_mapping, table_states)
    }

    pub async fn load_table_and_index_metadata(
        persistence_snapshot: &PersistenceSnapshot,
    ) -> anyhow::Result<(
        TableMapping,
        OrdMap<TableId, TableState>,
        IndexRegistry,
        BTreeMap<ResolvedDocumentId, (Timestamp, ResolvedDocument)>,
        BootstrapMetadata,
    )> {
        let _timer = metrics::load_table_and_index_metadata_timer();
        let bootstrap_metadata = Self::get_meta_ids(persistence_snapshot.persistence()).await?;
        let BootstrapMetadata {
            tables_by_id,
            index_by_id,
            tables_table_id,
            index_table_id,
        }: BootstrapMetadata = bootstrap_metadata;

        let index_documents =
            Self::load_raw_table_documents(persistence_snapshot, index_by_id, index_table_id)
                .await?;
        let table_documents = Self::load_table_documents::<TableMetadata>(
            persistence_snapshot,
            tables_by_id,
            tables_table_id,
        )
        .await?;

        let (table_mapping, table_states) = Self::table_mapping_and_states(table_documents);

        let persistence_version = persistence_snapshot.persistence().version();
        let index_registry = IndexRegistry::bootstrap(
            &table_mapping,
            index_documents.values().map(|(_, d)| d),
            persistence_version,
        )?;
        Ok((
            table_mapping,
            table_states,
            index_registry,
            index_documents,
            bootstrap_metadata,
        ))
    }

    pub fn virtual_table_mapping(
        virtual_tables: Vec<ParsedDocument<VirtualTableMetadata>>,
    ) -> VirtualTableMapping {
        let mut virtual_table_mapping = VirtualTableMapping::new();
        for virtual_table in virtual_tables {
            virtual_table_mapping.insert(virtual_table.number, virtual_table.name.clone());
        }
        virtual_table_mapping
    }

    pub async fn load_table_registry(
        persistence_snapshot: &PersistenceSnapshot,
        table_mapping: TableMapping,
        table_states: OrdMap<TableId, TableState>,
        index_registry: &IndexRegistry,
    ) -> anyhow::Result<TableRegistry> {
        let load_virtual_tables_timer = load_virtual_table_metadata_timer();
        let virtual_tables_table = table_mapping.id(&VIRTUAL_TABLES_TABLE)?;
        let virtual_tables_by_id = index_registry
            .must_get_by_id(virtual_tables_table.table_id)?
            .id();
        let virtual_tables = Self::load_table_documents::<VirtualTableMetadata>(
            persistence_snapshot,
            virtual_tables_by_id,
            virtual_tables_table.table_id,
        )
        .await?;
        let virtual_table_mapping = Self::virtual_table_mapping(virtual_tables);
        drop(load_virtual_tables_timer);

        let table_registry = TableRegistry::bootstrap(
            table_mapping,
            table_states,
            persistence_snapshot.persistence().version(),
            virtual_table_mapping,
        )?;
        Self::verify_invariants(&table_registry, index_registry)?;
        Ok(table_registry)
    }

    pub fn table_iterator<RT: Runtime>(&self, runtime: RT) -> TableIterator<RT> {
        TableIterator::new(
            runtime,
            self.timestamp(),
            self.persistence_reader.clone(),
            self.retention_validator.clone(),
            1000,
            None,
        )
    }

    pub async fn full_table_scan<'a, RT: Runtime>(
        &'a self,
        runtime: &RT,
        table_id: TableId,
        rate_limiter: &'a RateLimiter<RT>,
    ) -> anyhow::Result<LatestDocumentStream<'a>> {
        let table_by_id = self.index_registry().must_get_by_id(table_id)?.id();
        let table_iterator = self.table_iterator(runtime.clone());
        let stream =
            table_iterator.stream_documents_in_table(table_id, table_by_id, None, rate_limiter);
        Ok(stream.map_ok(|(document, ts)| (ts, document)).boxed())
    }

    /// Fetch _tables.by_id and _index.by_id for bootstrapping.
    pub async fn get_meta_ids(
        persistence: &dyn PersistenceReader,
    ) -> anyhow::Result<BootstrapMetadata> {
        let tables_by_id = persistence
            .get_persistence_global(PersistenceGlobalKey::TablesByIdIndex)
            .await?
            .context("missing _tables.by_id global")?
            .as_str()
            .context("_tables.by_id is not string")?
            .parse()?;
        let index_by_id = persistence
            .get_persistence_global(PersistenceGlobalKey::IndexByIdIndex)
            .await?
            .context("missing _index.by_id global")?
            .as_str()
            .context("_index.by_id is not string")?
            .parse()?;
        let tables_table_id: TableId = persistence
            .get_persistence_global(PersistenceGlobalKey::TablesTableId)
            .await?
            .context("missing _tables table ID global")?
            .as_str()
            .context("_tables table ID is not string")?
            .parse()?;
        let index_table_id = persistence
            .get_persistence_global(PersistenceGlobalKey::IndexTableId)
            .await?
            .context("missing _index table ID global")?
            .as_str()
            .context("_index table ID is not string")?
            .parse()?;
        Ok(BootstrapMetadata {
            tables_by_id,
            index_by_id,
            tables_table_id,
            index_table_id,
        })
    }

    pub async fn load<RT: Runtime>(
        rt: &RT,
        persistence: Arc<dyn PersistenceReader>,
        snapshot: RepeatableTimestamp,
        retention_validator: Arc<dyn RetentionValidator>,
    ) -> anyhow::Result<Self> {
        let repeatable_persistence: RepeatablePersistence =
            RepeatablePersistence::new(persistence.clone(), snapshot, retention_validator.clone());
        let persistence_snapshot =
            repeatable_persistence.read_snapshot(repeatable_persistence.upper_bound())?;

        // Step 1: Fetch tables and indexes from persistence.
        tracing::info!("Bootstrapping indexes...");
        let (table_mapping, table_states, index_registry, index_documents, bootstrap_metadata) =
            Self::load_table_and_index_metadata(&persistence_snapshot).await?;

        // Step 2: Load bootstrap tables indexes into memory.
        let load_indexes_into_memory_timer = load_indexes_into_memory_timer();
        let in_memory_indexes = {
            let mut index =
                BackendInMemoryIndexes::bootstrap(&index_registry, index_documents, *snapshot)?;
            index
                .load_enabled_for_tables(
                    &index_registry,
                    &table_mapping,
                    &persistence_snapshot,
                    &bootstrap_system_tables()
                        .iter()
                        .map(|t| t.table_name().clone())
                        .collect(),
                )
                .await?;
            index
        };
        drop(load_indexes_into_memory_timer);

        let search = SearchIndexManager::new(
            SearchIndexManagerState::Bootstrapping,
            persistence.version(),
        );
        let vector = VectorIndexManager::bootstrap_index_metadata(&index_registry)?;

        // Step 3: Stream document changes since the last table summary snapshot so they
        // are up to date.
        tracing::info!("Bootstrapping table summaries...");
        let (table_summary_snapshot, summaries_num_rows) = table_summary::bootstrap(
            rt,
            persistence.clone(),
            retention_validator.clone(),
            snapshot,
            false,
        )
        .await?;
        let table_summaries = TableSummaries::new(table_summary_snapshot.clone(), &table_mapping);
        tracing::info!("Bootstrapped table summaries (read {summaries_num_rows} rows)");

        // Step 4: Bootstrap our database metadata from the `_tables` documents and
        // computed table summaries.
        tracing::info!("Bootstrapping table metadata...");
        let table_registry = Self::load_table_registry(
            &persistence_snapshot,
            table_mapping,
            table_states,
            &index_registry,
        )
        .await?;

        Ok(Self {
            ts: persistence_snapshot.timestamp(),
            bootstrap_metadata,
            snapshot: Snapshot {
                table_registry,
                table_summaries,
                index_registry,
                in_memory_indexes,
                search_indexes: search,
                vector_indexes: vector,
            },
            persistence_snapshot,

            summaries_num_rows,

            persistence_reader: persistence,
            retention_validator,
        })
    }

    pub fn timestamp(&self) -> RepeatableTimestamp {
        self.ts
    }

    pub fn verify_invariants(
        table_registry: &TableRegistry,
        index_registry: &IndexRegistry,
    ) -> anyhow::Result<()> {
        let _timer = verify_invariants_timer();
        // Verify that all tables have table scan index.
        for (table_id, _, table_name) in table_registry.table_mapping().iter() {
            anyhow::ensure!(
                index_registry
                    .get_enabled(&TabletIndexName::by_id(table_id))
                    .is_some(),
                "Missing `by_id` index for {}",
                table_name,
            );
        }

        // Verify that all indexes are defined on tables that exist.
        for table_id in index_registry.all_tables_with_indexes() {
            anyhow::ensure!(
                table_registry.table_mapping().table_id_exists(table_id),
                "Table {:?} is missing but has one or more indexes",
                table_id,
            );
        }

        Ok(())
    }

    pub fn table_registry(&self) -> &TableRegistry {
        &self.snapshot.table_registry
    }

    pub fn index_registry(&self) -> &IndexRegistry {
        &self.snapshot.index_registry
    }

    pub fn table_summaries(&self) -> &TableSummaries {
        &self.snapshot.table_summaries
    }
}

// Used by the database to signal it has encountered a fatal error.
#[derive(Clone)]
pub struct ShutdownSignal {
    shutdown_tx: Option<async_broadcast::Sender<Arc<anyhow::Error>>>,
}

impl ShutdownSignal {
    pub fn new(shutdown_tx: async_broadcast::Sender<Arc<anyhow::Error>>) -> Self {
        Self {
            shutdown_tx: Some(shutdown_tx),
        }
    }

    // Creates a new ShutdownSignal that panics when signaled.
    pub fn panic() -> Self {
        Self { shutdown_tx: None }
    }

    pub fn signal(&self, fatal_error: anyhow::Error) {
        if let Some(ref shutdown_tx) = self.shutdown_tx {
            _ = shutdown_tx.try_broadcast(Arc::new(fatal_error));
        } else {
            // We don't anyone to shutdown signal configured. Just panic.
            panic!("Shutting down due to fatal error: {}", fatal_error);
        }
    }
}

/// ShortBoxFuture<'_, 'a, 'b, T> is a future with a shorter lifetime.
/// It is equivalent to BoxFuture<'a + 'b, T>, working
/// around limitations of HRTBs and explicit lifetime bounds.
/// This is useful when wrapping async closures, where the closure returns a
/// future that depends on both:
/// 1. references in the enclosing scope with lifetime 'a.
/// 2. references in the closure's arguments with lifetime 'b.
/// For example:
///
/// async fn with_retries<'a>(&'a self, f: F)
/// where F: for<'b> Fn(&'b Transaction) -> ShortBoxFuture<'_, 'a, 'b, ()>
/// {
///     let tx = self.begin();
///     for i in 0..2 {
///         f(&tx).await;
///     }
/// }
///
/// async fn go(&self) {
///     let document = ResolvedDocument::new();
///     with_retries(|tx| ShortBoxFuture::new(async {
///         tx.get(document.id()).await;
///     })).await
/// }
pub struct ShortBoxFuture<'c, 'a: 'c, 'b: 'c, T>(
    pub BoxFuture<'c, T>,
    PhantomData<&'a ()>,
    PhantomData<&'b ()>,
);
impl<'c, 'a: 'c, 'b: 'c, T, F: Future<Output = T> + Send + 'c> From<F>
    for ShortBoxFuture<'c, 'a, 'b, T>
{
    fn from(f: F) -> Self {
        Self(Box::pin(f), PhantomData, PhantomData)
    }
}

impl<RT: Runtime> Database<RT> {
    pub async fn load(
        mut persistence: Arc<dyn Persistence>,
        runtime: RT,
        searcher: Arc<dyn Searcher>,
        shutdown: ShutdownSignal,
        virtual_system_mapping: VirtualSystemMapping,
        usage_events: Arc<dyn UsageEventLogger>,
    ) -> anyhow::Result<Self> {
        let _load_database_timer = metrics::load_database_timer();

        // Initialize the database if it's a new database.
        if persistence.is_fresh() {
            tracing::info!("Initializing database with system tables...");
            Self::initialize(&runtime, &mut persistence).await?;
        }

        // Load data into a DatabaseReader, including indexes and shapes.
        let reader = persistence.reader();

        let follower_retention_manager =
            FollowerRetentionManager::new(runtime.clone(), persistence.reader()).await?;

        // Get the latest timestamp to perform the load at.
        let snapshot_ts = new_idle_repeatable_ts(persistence.as_ref(), &runtime).await?;
        let original_max_ts = DatabaseSnapshot::max_ts(&*reader).await?;

        let db_snapshot = DatabaseSnapshot::load(
            &runtime,
            reader.clone(),
            snapshot_ts,
            Arc::new(follower_retention_manager.clone()),
        )
        .await?;
        let max_ts = DatabaseSnapshot::max_ts(&*reader).await?;
        anyhow::ensure!(
            original_max_ts == max_ts,
            "race while loading DatabaseSnapshot: max ts {original_max_ts} at start, {max_ts} at \
             end",
        );
        let DatabaseSnapshot {
            bootstrap_metadata,
            persistence_snapshot: _,
            ts,
            snapshot,
            summaries_num_rows,
            persistence_reader: _,
            retention_validator: _,
        } = db_snapshot;
        if summaries_num_rows > 0 {
            let table_summary_snapshot = TableSummarySnapshot {
                tables: snapshot
                    .table_summaries
                    .tables
                    .clone()
                    .into_iter()
                    .collect(),
                ts: *ts,
            };
            table_summary::write_snapshot(&*persistence, &table_summary_snapshot).await?;
        }

        let snapshot_manager = SnapshotManager::new(*ts, snapshot);
        let (snapshot_reader, snapshot_writer) = new_split_rw_lock(snapshot_manager);

        let retention_manager = LeaderRetentionManager::new(
            runtime.clone(),
            persistence.clone(),
            snapshot_reader.clone(),
            follower_retention_manager,
        )
        .await?;

        let persistence_reader = persistence.reader();
        let (log_owner, log_reader, log_writer) = new_write_log(*ts, persistence_reader.version());
        let subscriptions =
            SubscriptionsWorker::start(log_owner, runtime.clone(), persistence_reader.version());
        let usage_counter = UsageCounter::new(usage_events);
        let committer = Committer::start(
            log_writer,
            snapshot_writer,
            persistence,
            runtime.clone(),
            Arc::new(retention_manager.clone()),
            shutdown,
        );
        let database = Self {
            committer,
            subscriptions,
            runtime,
            log: log_reader,
            retention_manager,
            snapshot_manager: snapshot_reader,
            reader: persistence_reader.clone(),
            write_commits_since_load: Arc::new(AtomicUsize::new(0)),
            searcher,
            search_storage: Arc::new(OnceLock::new()),
            usage_counter,
            virtual_system_mapping,
            bootstrap_metadata,
        };

        Ok(database)
    }

    pub fn set_search_storage(&self, search_storage: Arc<dyn Storage>) {
        self.search_storage
            .set(search_storage.clone())
            .expect("Tried to set search storage more than once");
        tracing::info!("Set search storage to {search_storage:?}");
    }

    pub fn start_search_and_vector_bootstrap(&self, pause_client: PauseClient) -> RT::Handle {
        let worker = self.new_search_and_vector_bootstrap_worker(pause_client);
        self.runtime
            .spawn("search_and_vector_bootstrap", async move {
                worker.start().await
            })
    }

    #[cfg(test)]
    pub fn new_search_and_vector_bootstrap_worker_for_testing(
        &self,
    ) -> SearchAndVectorIndexBootstrapWorker<RT> {
        self.new_search_and_vector_bootstrap_worker(PauseClient::new())
    }

    fn new_search_and_vector_bootstrap_worker(
        &self,
        pause_client: PauseClient,
    ) -> SearchAndVectorIndexBootstrapWorker<RT> {
        let (ts, snapshot) = self.snapshot_manager.lock().latest();
        let vector_persistence =
            RepeatablePersistence::new(self.reader.clone(), ts, self.retention_validator());
        let table_mapping = snapshot.table_mapping().clone();
        SearchAndVectorIndexBootstrapWorker::new(
            self.runtime.clone(),
            snapshot.index_registry,
            vector_persistence,
            table_mapping,
            self.committer.clone(),
            pause_client,
        )
    }

    pub fn shutdown(&self) {
        self.committer.shutdown();
        self.subscriptions.shutdown();
        self.retention_manager.shutdown();
        tracing::info!("Database shutdown");
    }

    pub fn retention_validator(&self) -> Arc<dyn RetentionValidator> {
        Arc::new(self.retention_manager.clone())
    }

    /// Load the set of documents and tombstones in the given table between
    /// within the given timestamp.
    ///
    /// See PersistenceReader.load_documents_from_table for performance caveats!
    ///
    /// rate_limiter must be based on rows per second.
    pub fn load_documents_in_table<'a>(
        &'a self,
        table_id: TableId,
        timestamp_range: TimestampRange,
        rate_limiter: &'a RateLimiter<RT>,
    ) -> DocumentStream<'a> {
        self.reader
            .load_documents_from_table(
                table_id,
                timestamp_range,
                Order::Asc,
                *DEFAULT_DOCUMENTS_PAGE_SIZE,
                self.retention_validator(),
            )
            .then(|val| async {
                while let Err(not_until) = rate_limiter.check() {
                    let delay = not_until.wait_time_from(self.runtime.monotonic_now().as_nanos());
                    self.runtime.wait(delay).await;
                }
                val
            })
            .boxed()
    }

    /// See table_iterator.
    /// This is a convenience method if you have a table name and don't want to
    /// customize the page size.
    pub async fn iter_table_documents<'a, 'b>(
        &'a self,
        snapshot_ts: RepeatableTimestamp,
        table_name: &'a TableName,
        rate_limiter: &'b RateLimiter<RT>,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<ResolvedDocument>> + 'b> {
        let iterator = self.table_iterator(snapshot_ts, 100, None);
        let table_mapping = self.snapshot_table_mapping(snapshot_ts).await?;
        let table_id = table_mapping.id(table_name)?.table_id;
        let by_id_indexes = self.snapshot_by_id_indexes(snapshot_ts).await?;
        let by_id = *by_id_indexes
            .get(&table_id)
            .ok_or_else(|| anyhow::anyhow!("by_id not found for {table_name}"))?;
        let stream = iterator.stream_documents_in_table(table_id, by_id, None, rate_limiter);
        Ok(stream.map_ok(|(doc, _)| doc))
    }

    /// Allows iterating over tables at any repeatable timestamp,
    /// even if it's outside of retention.
    /// TableIterator will have to walk all documents between snapshot_ts
    /// and now, so it is inefficient for very old snapshots.
    pub fn table_iterator(
        &self,
        snapshot_ts: RepeatableTimestamp,
        page_size: usize,
        pause_client: Option<PauseClient>,
    ) -> TableIterator<RT> {
        let runtime = self.runtime.clone();
        let retention_validator = self.retention_validator();
        let persistence = self.reader.clone();
        TableIterator::new(
            runtime,
            snapshot_ts,
            persistence,
            retention_validator,
            page_size,
            pause_client,
        )
    }

    pub async fn snapshot_table_mapping(
        &self,
        ts: RepeatableTimestamp,
    ) -> anyhow::Result<TableMapping> {
        let table_iterator = self.table_iterator(ts, 100, None);
        let (_, snapshot) = self.snapshot_manager.lock().latest();
        let tables_table_id = snapshot
            .table_registry
            .table_mapping()
            .id(&TABLES_TABLE)?
            .table_id;
        let tables_by_id = snapshot
            .index_registry
            .must_get_by_id(tables_table_id)?
            .id();
        let rate_limiter =
            new_rate_limiter(self.runtime.clone(), Quota::per_second(1000.try_into()?));
        let stream = table_iterator.stream_documents_in_table(
            tables_table_id,
            tables_by_id,
            None,
            &rate_limiter,
        );
        pin_mut!(stream);
        let mut table_mapping = TableMapping::new();
        while let Some((table_doc, _)) = stream.try_next().await? {
            let table_doc: ParsedDocument<TableMetadata> = table_doc.try_into()?;
            if table_doc.is_active() {
                table_mapping.insert(
                    TableId(table_doc.id().internal_id()),
                    table_doc.number,
                    table_doc.into_value().name,
                );
            }
        }
        Ok(table_mapping)
    }

    pub async fn snapshot_by_id_indexes(
        &self,
        ts: RepeatableTimestamp,
    ) -> anyhow::Result<BTreeMap<TableId, IndexId>> {
        let table_iterator = self.table_iterator(ts, 100, None);
        let (_, snapshot) = self.snapshot_manager.lock().latest();
        let index_table_id = snapshot.index_registry.index_table();
        let index_by_id = snapshot
            .index_registry
            .must_get_by_id(index_table_id.table_id)?
            .id();
        let rate_limiter =
            new_rate_limiter(self.runtime.clone(), Quota::per_second(1000.try_into()?));
        let stream = table_iterator.stream_documents_in_table(
            index_table_id.table_id,
            index_by_id,
            None,
            &rate_limiter,
        );
        pin_mut!(stream);
        let mut by_id_indexes = BTreeMap::new();
        while let Some((index_doc, _)) = stream.try_next().await? {
            let index_doc = TabletIndexMetadata::from_document(index_doc)?;
            if index_doc.name.is_by_id() {
                by_id_indexes.insert(*index_doc.name.table(), index_doc.id().internal_id());
            }
        }
        Ok(by_id_indexes)
    }

    async fn initialize(rt: &RT, persistence: &mut Arc<dyn Persistence>) -> anyhow::Result<()> {
        let mut id_generator = TransactionIdGenerator::new(rt)?;
        let ts = rt.generate_timestamp()?;
        let mut creation_time = CreationTime::try_from(ts)?;
        let mut document_writes = vec![];

        let mut system_by_id = BTreeMap::new();
        let mut table_mapping = TableMapping::new();

        // Step 0: Generate document ids for bootstrapping database system tables.
        for table in bootstrap_system_tables() {
            let table_name = table.table_name();
            let table_number = *DEFAULT_BOOTSTRAP_TABLE_NUMBERS
                .get(table_name)
                .context(format!("Table name {table_name} not found"))?;
            let table_id = TableId(id_generator.generate_internal());
            let existing_tn = table_mapping.name_by_number_if_exists(table_number);
            anyhow::ensure!(
                existing_tn.is_none(),
                "{table_number} is used by both {table_name} and {existing_tn:?}"
            );
            anyhow::ensure!(
                table_number < TableNumber::try_from(NUM_RESERVED_SYSTEM_TABLE_NUMBERS)?,
                "{table_number} picked for system table {table_name} is reserved for user tables"
            );
            anyhow::ensure!(
                table_number >= TableNumber::try_from(NUM_RESERVED_LEGACY_TABLE_NUMBERS)?,
                "{table_number} picked for system table {table_name} is reserved for legacy tables"
            );
            table_mapping.insert(table_id, table_number, table_name.clone());
        }

        // Get table ids for tables we will be populating.
        let tables_table_id = table_mapping.name_to_id()(TABLES_TABLE.clone())?;
        let index_table_id = table_mapping.name_to_id()(INDEX_TABLE.clone())?;

        persistence
            .write_persistence_global(
                PersistenceGlobalKey::TablesTableId,
                tables_table_id.table_id.to_string().into(),
            )
            .await?;
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::IndexTableId,
                index_table_id.table_id.to_string().into(),
            )
            .await?;

        // Step 1: Generate documents.
        // Create bootstrap system table values.
        for table in bootstrap_system_tables() {
            let table_name = table.table_name();
            let table_id = table_mapping.id(table_name)?;
            let document_id: GenericDocumentId<TableIdAndTableNumber> =
                tables_table_id.id(table_id.table_id.0);
            let metadata = TableMetadata::new(table_name.clone(), table_id.table_number);
            let document = ResolvedDocument::new(
                document_id,
                creation_time.increment()?,
                metadata.try_into()?,
            )?;
            document_writes.push((document_id, document));

            // Create the default `by_id` index. Since the table is created just now there
            // is no need to backfill.
            let index_id = id_generator.generate(&index_table_id);
            system_by_id.insert(table_name.clone(), index_id.internal_id());
            let metadata = IndexMetadata::new_enabled(
                GenericIndexName::by_id(table_id.table_id),
                IndexedFields::by_id(),
            );
            let document =
                ResolvedDocument::new(index_id, creation_time.increment()?, metadata.try_into()?)?;
            document_writes.push((index_id, document));

            // Create the `by_creation_time` index for all tables except "_index", which can
            // only have the "by_id" index.
            if table_name != &*INDEX_TABLE {
                let index_id = id_generator.generate(&index_table_id);
                let metadata = IndexMetadata::new_enabled(
                    GenericIndexName::by_creation_time(table_id.table_id),
                    IndexedFields::creation_time(),
                );
                let document = ResolvedDocument::new(
                    index_id,
                    creation_time.increment()?,
                    metadata.try_into()?,
                )?;
                document_writes.push((index_id, document));
            }
        }

        // Create system indexes.
        for SystemIndex { name, fields } in bootstrap_system_tables()
            .into_iter()
            .flat_map(|t| t.indexes())
        {
            let name = name.map_table(&table_mapping.name_to_id())?.into();
            let document_id = id_generator.generate(&index_table_id);
            let index_metadata = IndexMetadata::new_enabled(name, fields);
            let document = ResolvedDocument::new(
                document_id,
                creation_time.increment()?,
                index_metadata.try_into()?,
            )?;
            document_writes.push((document_id, document));
        }

        // Step 2: Generate indexes updates.
        // Build the index metadata from the index documents.
        let index_documents = document_writes
            .iter()
            .filter(|(id, _)| id.table() == &index_table_id)
            .map(|(id, doc)| (*id, (ts, doc.clone())))
            .collect::<BTreeMap<_, _>>();
        let mut index_registry = IndexRegistry::bootstrap(
            &table_mapping,
            index_documents.values().map(|(_, d)| d),
            persistence.reader().version(),
        )?;
        let mut in_memory_indexes =
            BackendInMemoryIndexes::bootstrap(&index_registry, index_documents, ts)?;

        // Compute the necessary index updates by feeding the remaining documents.
        let mut index_writes = Vec::new();
        for (_id, doc) in &document_writes {
            index_registry.update(None, Some(doc))?;
            let updates = in_memory_indexes.update(&index_registry, ts, None, Some(doc.clone()));
            index_writes.extend(updates);
        }

        // Step 3: Add timestamp and write everything to persistence.
        let ts = Timestamp::MIN;
        let document_writes = document_writes
            .into_iter()
            .map(|(id, doc)| (ts, id.into(), Some(doc)))
            .collect();
        let index_writes = index_writes
            .into_iter()
            .map(|update| (ts, update))
            .collect();

        // Write _tables.by_id and _index.by_id to persistence globals for
        // bootstrapping.
        let tables_by_id = *system_by_id
            .get(&TABLES_TABLE)
            .expect("_tables.by_id should exist");
        let index_by_id = *system_by_id
            .get(&INDEX_TABLE)
            .expect("_index.by_id should exist");
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::TablesByIdIndex,
                tables_by_id.to_string().into(),
            )
            .await?;
        persistence
            .write_persistence_global(
                PersistenceGlobalKey::IndexByIdIndex,
                index_by_id.to_string().into(),
            )
            .await?;

        // Write directly to persistence.
        // This is a little unsafe because we generated random IDs for this documents
        // with `TransactionIdGenerator`, but aren't using a real `Transaction` so we
        // don't have our usual protections against ID collisions.
        // Our `ConflictStrategy::Error` should notice the problem but consider
        // improving in the future (CX-2265).
        persistence
            .write(document_writes, index_writes, ConflictStrategy::Error)
            .await?;
        Ok(())
    }

    pub fn persistence_version(&self) -> PersistenceVersion {
        self.reader.version()
    }

    pub fn now_ts_for_reads(&self) -> RepeatableTimestamp {
        let snapshot_manager = self.snapshot_manager.lock();
        snapshot_manager.latest_ts()
    }

    pub async fn begin_system(&self) -> anyhow::Result<Transaction<RT>> {
        self.begin(Identity::system()).await
    }

    pub async fn execute_with_retries<'a, T, R, F>(
        &'a self,
        identity: Identity,
        max_failures: u32,
        mut backoff: Backoff,
        usage: FunctionUsageTracker,
        is_retriable: R,
        mut pause_client: PauseClient,
        write_source: impl Into<WriteSource>,
        f: F,
    ) -> anyhow::Result<(Timestamp, T, OccRetryStats)>
    where
        T: Send,
        R: Fn(&Error) -> bool,
        F: for<'b> Fn(&'b mut Transaction<RT>) -> ShortBoxFuture<'_, 'a, 'b, anyhow::Result<T>>,
    {
        let write_source = write_source.into();
        let result = {
            let mut error = None;
            while backoff.failures() < max_failures {
                let mut tx = self
                    .begin_with_usage(identity.clone(), usage.clone())
                    .await?;
                pause_client.wait("retry_tx_loop_start").await;
                let start = Instant::now();
                let result = async {
                    let t = f(&mut tx).0.await?;
                    let func_end_time = Instant::now();
                    let ts = self
                        .commit_with_write_source(tx, write_source.clone())
                        .await?;
                    let commit_end_time = Instant::now();
                    Ok((ts, t, func_end_time, commit_end_time))
                }
                .await;
                let total_duration = Instant::now() - start;
                match result {
                    Err(e) => {
                        if is_retriable(&e) {
                            let delay = self.runtime.with_rng(|rng| backoff.fail(rng));
                            tracing::warn!("Retrying transaction after error: {}", e);
                            self.runtime.wait(delay).await;
                            error = Some(e);
                            continue;
                        } else {
                            return Err(e);
                        }
                    },
                    Ok((ts, t, func_end_time, commit_end_time)) => {
                        return Ok((
                            ts,
                            t,
                            OccRetryStats {
                                retries: backoff.failures(),
                                total_duration,
                                duration: func_end_time - start,
                                commit_duration: commit_end_time - func_end_time,
                            },
                        ))
                    },
                }
            }
            let error =
                error.unwrap_or_else(|| anyhow::anyhow!("Error was not returned from commit"));
            Err(error)
        };
        pause_client.close("retry_tx_loop_start");
        result
    }

    pub async fn execute_with_occ_retries<'a, T, F>(
        &'a self,
        identity: Identity,
        usage: FunctionUsageTracker,
        pause_client: PauseClient,
        write_source: impl Into<WriteSource>,
        f: F,
    ) -> anyhow::Result<(Timestamp, T, OccRetryStats)>
    where
        T: Send,
        F: for<'b> Fn(&'b mut Transaction<RT>) -> ShortBoxFuture<'_, 'a, 'b, anyhow::Result<T>>,
    {
        let backoff = Backoff::new(INITIAL_OCC_BACKOFF, MAX_OCC_BACKOFF);
        let is_retriable = |e: &Error| e.is_occ();
        self.execute_with_retries(
            identity,
            MAX_OCC_FAILURES,
            backoff,
            usage,
            is_retriable,
            pause_client,
            write_source,
            f,
        )
        .await
    }

    pub async fn begin(&self, identity: Identity) -> anyhow::Result<Transaction<RT>> {
        self.begin_with_usage(identity, FunctionUsageTracker::new())
            .await
    }

    pub async fn begin_with_usage(
        &self,
        identity: Identity,
        usage: FunctionUsageTracker,
    ) -> anyhow::Result<Transaction<RT>> {
        let ts = self.now_ts_for_reads();
        self.begin_with_repeatable_ts(identity, ts, usage).await
    }

    pub async fn begin_with_ts(
        &self,
        identity: Identity,
        ts: Timestamp,
        usage_tracker: FunctionUsageTracker,
    ) -> anyhow::Result<Transaction<RT>> {
        let ts = {
            let snapshot_manager = self.snapshot_manager.lock();
            snapshot_manager.latest_ts().prior_ts(ts)?
        };
        self.begin_with_repeatable_ts(identity, ts, usage_tracker)
            .await
    }

    async fn begin_with_repeatable_ts(
        &self,
        identity: Identity,
        repeatable_ts: RepeatableTimestamp,
        usage_tracker: FunctionUsageTracker,
    ) -> anyhow::Result<Transaction<RT>> {
        let latest_ts = self.now_ts_for_reads();
        if repeatable_ts > latest_ts {
            anyhow::bail!(
                "Timestamp {} beyond now_ts_for_reads {}",
                repeatable_ts,
                latest_ts
            );
        }
        let snapshot = self.snapshot_manager.lock().snapshot(*repeatable_ts)?;

        // TODO: Use `begin_ts` outside of just the "_creationTime".
        let begin_ts = cmp::max(latest_ts.succ()?, self.runtime.generate_timestamp()?);
        let creation_time = CreationTime::try_from(begin_ts)?;
        let id_generator = TransactionIdGenerator::new(&self.runtime)?;
        let transaction_index = TransactionIndex::new(
            snapshot.index_registry.clone(),
            DatabaseIndexSnapshot::new(
                snapshot.index_registry.clone(),
                Arc::new(snapshot.in_memory_indexes),
                snapshot.table_registry.table_mapping().clone(),
                RepeatablePersistence::new(
                    self.reader.clone(),
                    repeatable_ts,
                    Arc::new(self.retention_manager.clone()),
                )
                .read_snapshot(repeatable_ts)?,
            ),
            Arc::new(SearchIndexManagerSnapshot::new(
                snapshot.index_registry,
                snapshot.search_indexes,
                self.searcher.clone(),
                self.search_storage.clone(),
            )),
        );
        let count_snapshot = Arc::new(snapshot.table_summaries);
        let tx = Transaction::new(
            identity,
            id_generator,
            creation_time,
            transaction_index,
            snapshot.table_registry,
            count_snapshot,
            self.runtime.clone(),
            usage_tracker,
            Arc::new(self.retention_manager.clone()),
            self.virtual_system_mapping.clone(),
        );
        Ok(tx)
    }

    pub fn snapshot(&self, ts: RepeatableTimestamp) -> anyhow::Result<Snapshot> {
        self.snapshot_manager.lock().snapshot(*ts)
    }

    pub fn latest_snapshot(&self) -> anyhow::Result<Snapshot> {
        let snapshot = self.snapshot_manager.lock().latest_snapshot();
        Ok(snapshot)
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn commit(&self, transaction: Transaction<RT>) -> anyhow::Result<Timestamp> {
        self.commit_with_write_source(transaction, WriteSource::unknown())
            .await
    }

    #[minitrace::trace]
    pub async fn commit_with_write_source(
        &self,
        transaction: Transaction<RT>,
        write_source: impl Into<WriteSource>,
    ) -> anyhow::Result<Timestamp> {
        let readonly = transaction.is_readonly();
        let result = self
            .committer
            .commit(transaction, write_source.into())
            .in_span(
                SpanContext::current_local_parent()
                    .map(|ctx| {
                        Span::root(format!("{}::CommitterClient::commit", full_name!()), ctx)
                    })
                    .unwrap_or(Span::noop()),
            )
            .await?;
        if !readonly {
            self.write_commits_since_load.fetch_add(1, Ordering::SeqCst);
        }
        Ok(result)
    }

    pub async fn load_indexes_into_memory(
        &self,
        tables: BTreeSet<TableName>,
    ) -> anyhow::Result<()> {
        self.committer.load_indexes_into_memory(tables).await
    }

    #[cfg(any(test, feature = "testing"))]
    pub async fn bump_max_repeatable_ts(&self) -> anyhow::Result<Timestamp> {
        self.committer.bump_max_repeatable_ts().await
    }

    pub fn write_commits_since_load(&self) -> usize {
        self.write_commits_since_load.load(Ordering::SeqCst)
    }

    pub async fn subscribe(&self, token: Token) -> anyhow::Result<Subscription> {
        self.subscriptions.subscribe(token).await
    }

    fn user_table_filter(table_filter: &Option<TableName>, table: &TableName) -> bool {
        if table.is_system() {
            return false;
        }
        if let Some(table_filter) = table_filter {
            table_filter == table
        } else {
            true
        }
    }

    pub async fn document_deltas(
        &self,
        identity: Identity,
        cursor: Option<Timestamp>,
        table_filter: Option<TableName>,
        rows_read_limit: usize,
        rows_returned_limit: usize,
    ) -> anyhow::Result<DocumentDeltas> {
        anyhow::ensure!(
            identity.is_system() || identity.is_admin(),
            unauthorized_error("document_deltas")
        );
        anyhow::ensure!(rows_read_limit >= rows_returned_limit);
        let (upper_bound, table_mapping) = {
            let mut tx = self.begin(identity).await?;
            (tx.begin_timestamp(), tx.table_mapping().clone())
        };
        let repeatable_persistence = RepeatablePersistence::new(
            self.reader.clone(),
            upper_bound,
            self.retention_validator(),
        );
        let range = match cursor {
            Some(ts) => TimestampRange::new((Bound::Excluded(ts), Bound::Unbounded))?,
            None => TimestampRange::all(),
        };
        let mut document_stream = repeatable_persistence.load_documents(range, Order::Asc);
        // deltas accumulated in (ts, id) order to return.
        let mut deltas = vec![];
        // new_cursor is set once, when we know the final timestamp.
        let mut new_cursor = None;
        // has_more indicates there are more documents in the stream so the caller
        // should request another page.
        let mut has_more = false;
        let mut rows_read = 0;
        while let Some((ts, id, maybe_doc)) = match document_stream.try_next().await {
            Ok::<_, Error>(doc) => doc,
            Err(e) if e.is_out_of_retention() => {
                // Throws a user error if the documents window is out of retention
                anyhow::bail!(ErrorMetadata::bad_request(
                    "InvalidWindowToReadDocuments",
                    format!("Timestamp {} is too old", range.min_timestamp_inclusive())
                ))
            },
            Err(e) => anyhow::bail!(e),
        } {
            rows_read += 1;
            if let Some(new_cursor) = new_cursor
                && new_cursor < ts
            {
                // If we determined new_cursor already, we know the maximum ts we want to
                // return. So if we read a document with a higher ts, we are
                // done.
                has_more = true;
                break;
            }
            if new_cursor.is_none() && rows_read >= rows_read_limit {
                // We want to finish, but we have to process all documents at this timestamp.
                new_cursor = Some(ts);
            }
            // Skip deltas for system and non-selected tables.
            let Ok(table_name) = table_mapping.tablet_name(*id.table()) else {
                // Ignore the row if it comes from a deleted table
                continue;
            };
            let id: DocumentIdV6 = id.map_table(table_mapping.inject_table_number())?.into();
            if Self::user_table_filter(&table_filter, &table_name) {
                deltas.push((ts, id, table_name, maybe_doc));
                if new_cursor.is_none() && deltas.len() >= rows_returned_limit {
                    // We want to finish, but we have to process all documents at this timestamp.
                    new_cursor = Some(ts);
                }
            }
        }
        Ok(DocumentDeltas {
            deltas,
            // If new_cursor is still None, we exhausted the stream.
            cursor: new_cursor.unwrap_or(*upper_bound),
            has_more,
        })
    }

    pub async fn list_snapshot(
        &self,
        identity: Identity,
        snapshot: Option<Timestamp>,
        cursor: Option<DocumentIdV6>,
        table_filter: Option<TableName>,
        rows_read_limit: usize,
        rows_returned_limit: usize,
    ) -> anyhow::Result<SnapshotPage> {
        anyhow::ensure!(
            identity.is_system() || identity.is_admin(),
            unauthorized_error("list_snapshot")
        );
        anyhow::ensure!(rows_read_limit >= rows_returned_limit);
        let snapshot = match snapshot {
            Some(ts) => self.now_ts_for_reads().prior_ts(ts)?,
            None => self.now_ts_for_reads(),
        };
        let table_mapping = self.snapshot_table_mapping(snapshot).await?;
        let by_id_indexes = self.snapshot_by_id_indexes(snapshot).await?;
        let cursor = cursor
            .map(|c| c.map_table(table_mapping.inject_table_id()))
            .transpose()?;
        let table_numbers: BTreeSet<_> = table_mapping
            .iter()
            .filter(|(_, table_number, name)| {
                Self::user_table_filter(&table_filter, name)
                    && cursor
                        .as_ref()
                        .map(|c| table_number >= &c.table().table_number)
                        .unwrap_or(true)
            })
            .map(|(_, table_number, _)| table_number)
            .collect();
        let mut table_numbers = table_numbers.into_iter();
        let table_id = match table_numbers.next() {
            Some(first_table) => table_mapping.inject_table_id()(first_table)?.table_id,
            None => {
                return Ok(SnapshotPage {
                    documents: vec![],
                    snapshot: *snapshot,
                    cursor: None,
                    has_more: false,
                });
            },
        };
        let by_id = *by_id_indexes
            .get(&table_id)
            .ok_or_else(|| anyhow::anyhow!("by_id index for {table_id:?} missing"))?;
        let table_iterator = self.table_iterator(snapshot, 100, None);
        let rate_limiter =
            new_rate_limiter(self.runtime.clone(), Quota::per_second(100.try_into()?));
        let document_stream =
            table_iterator.stream_documents_in_table(table_id, by_id, cursor, &rate_limiter);
        pin_mut!(document_stream);
        // new_cursor is set once, when we know the final internal_id.
        let mut new_cursor = None;
        // documents accumulated in (ts, id) order to return.
        let mut documents = vec![];
        let mut rows_read = 0;
        while let Some((doc, ts)) = document_stream.try_next().await? {
            rows_read += 1;
            let id = doc.id_v6();
            let table_name = table_mapping.tablet_name(doc.id().table().table_id)?;
            documents.push((ts, table_name, doc));
            if rows_read >= rows_read_limit || documents.len() >= rows_returned_limit {
                new_cursor = Some(id);
                break;
            }
        }
        let new_cursor = new_cursor.or_else(|| match table_numbers.next() {
            Some(next_table_number) => Some(DocumentIdV6::new(next_table_number, InternalId::MIN)),
            None => None,
        });
        let has_more = new_cursor.is_some();
        Ok(SnapshotPage {
            documents,
            snapshot: *snapshot,
            cursor: new_cursor,
            has_more,
        })
    }

    #[cfg(test)]
    pub fn table_names(&self, identity: Identity) -> anyhow::Result<BTreeSet<TableName>> {
        if !(identity.is_admin() || identity.is_system()) {
            anyhow::bail!(unauthorized_error("table_names"));
        }
        Ok(self
            .snapshot_manager
            .lock()
            .latest_snapshot()
            .table_registry
            .user_table_names()
            .cloned()
            .collect())
    }

    /// Attempt to pull a token forward to a given timestamp, returning `None`
    /// if there have been overlapping writes between the token's original
    /// timestamp and `ts`.
    pub async fn refresh_token(
        &self,
        token: Token,
        ts: Timestamp,
    ) -> anyhow::Result<Option<Token>> {
        let _timer = metrics::refresh_token_timer();
        self.log.refresh_token(token, ts)
    }

    pub fn memory_consistency_check(&self) -> anyhow::Result<()> {
        let snapshot = self.snapshot_manager.lock().latest_snapshot();
        snapshot.search_indexes.consistency_check()?;
        Ok(())
    }

    pub fn get_vector_index_storage(
        &self,
        identity: Identity,
    ) -> anyhow::Result<BTreeMap<String, u64>> {
        if !(identity.is_admin() || identity.is_system()) {
            anyhow::bail!(unauthorized_error("get_vector_index_storage"));
        }
        let (_, snapshot) = self.snapshot_manager.lock().latest();
        let table_mapping = snapshot.table_registry.table_mapping().clone();
        let index_registry = snapshot.index_registry;
        Iterator::try_fold(
            &mut index_registry.all_vector_indexes().into_iter(),
            BTreeMap::new(),
            |mut map, index| {
                let (_, value) = index.into_id_and_value();
                let table_id = *value.name.table();
                let table_name = table_mapping.tablet_name(table_id)?.deref().into();
                let size = value.config.estimate_pricing_size_bytes()?;
                map.entry(table_name)
                    .and_modify(|sum| *sum += size)
                    .or_insert(size);
                Ok(map)
            },
        )
    }

    pub fn get_user_document_and_index_storage(
        &self,
        identity: Identity,
    ) -> anyhow::Result<BTreeMap<TableName, (usize, usize)>> {
        if !(identity.is_admin() || identity.is_system()) {
            anyhow::bail!(unauthorized_error("get_user_document_storage"));
        }

        let (_, snapshot) = self.snapshot_manager.lock().latest();
        let table_mapping = snapshot.table_mapping().clone();

        let mut document_storage_by_table = BTreeMap::new();
        for (table_name, summary) in snapshot.iter_user_table_summaries() {
            let table_size = summary.total_size_rounded() as usize;
            document_storage_by_table.insert(table_name, (table_size, 0));
        }

        // TODO: We are currently using document size * index count as a rough
        // approximation for how much storage indexes use, but we should fix this to
        // only charge for the fields that are indexed.
        for index in snapshot.index_registry.all_enabled_indexes() {
            let index_name = index
                .name
                .clone()
                .map_table(&table_mapping.tablet_to_name())
                .unwrap();
            let table_name = index_name.table().clone();

            if !index_name.is_system_owned() {
                let (document_size, index_size) = *document_storage_by_table
                    .get(&table_name)
                    .expect("Index on a nonexistent table");
                document_storage_by_table
                    .insert(table_name, (document_size, index_size + document_size));
            }
        }

        Ok(document_storage_by_table)
    }

    pub fn usage_counter(&self) -> UsageCounter {
        self.usage_counter.clone()
    }

    pub fn write_log_size(&self) -> usize {
        self.log.heap_size()
    }

    pub fn search_storage(&self) -> Arc<dyn Storage> {
        self.search_storage
            .get()
            .expect("search_storage not initialized")
            .clone()
    }

    pub async fn vector_search(
        &self,
        _identity: Identity,
        query: VectorSearch,
    ) -> anyhow::Result<(Vec<PublicVectorSearchQueryResult>, FunctionUsageStats)> {
        let mut last_error = None;
        let mut backoff = Backoff::new(INITIAL_VECTOR_BACKOFF, MAX_VECTOR_BACKOFF);
        let timer = vector_search_with_retries_timer();
        while backoff.failures() < MAX_VECTOR_ATTEMPTS {
            let ts = self.now_ts_for_reads();
            match self.vector_search_at_ts(query.clone(), ts).await {
                Err(e) => {
                    // If backend hasn't loaded the in-memory index yet, it returns
                    // overloaded. We want to retry those.
                    if e.is_overloaded() {
                        let delay = self.runtime.with_rng(|rng| backoff.fail(rng));
                        last_error = Some(e);
                        if backoff.failures() >= MAX_VECTOR_ATTEMPTS {
                            break;
                        }
                        tracing::warn!(
                            "Retrying vector search error: {}",
                            last_error.as_ref().unwrap()
                        );
                        self.runtime.wait(delay).await;
                        continue;
                    } else {
                        timer.finish(false);
                        return Err(e);
                    }
                },
                Ok(result) => {
                    timer.finish(true);
                    return Ok(result);
                },
            }
        }
        let last_error = last_error.expect("Exited vector_search() loop without any failure");
        timer.finish(false);
        Err(last_error)
    }

    pub async fn vector_search_at_ts(
        &self,
        query: VectorSearch,
        ts: RepeatableTimestamp,
    ) -> anyhow::Result<(Vec<PublicVectorSearchQueryResult>, FunctionUsageStats)> {
        let timer = metrics::vector::vector_search_timer();
        let usage = FunctionUsageTracker::new();
        let snapshot = self.snapshot(ts)?;
        let table_mapping = snapshot.table_mapping();
        if !table_mapping.name_exists(query.index_name.table()) {
            return Ok((vec![], usage.gather_user_stats()));
        }
        let table_number = table_mapping.id(query.index_name.table())?.table_number;
        let index_name = query
            .index_name
            .clone()
            .to_resolved(table_mapping.name_to_id())?
            .into();
        let index = snapshot
            .index_registry
            .require_enabled(&index_name, &query.index_name)?;
        let resolved: vector::InternalVectorSearch = query.resolve(table_mapping)?;
        let search_storage = self.search_storage();
        let results: Vec<_> = snapshot
            .vector_indexes
            .vector_search(
                &index,
                resolved,
                self.searcher.clone(),
                search_storage.clone(),
            )
            .await?
            .into_iter()
            .map(|r| r.to_public(table_number))
            .collect();
        let size: u64 = results.iter().map(|row| row.size() as u64).sum();
        usage.track_vector_egress_size(
            table_mapping.tablet_name(*index_name.table())?.to_string(),
            size,
            // We don't have system owned vector indexes.
            false,
        );
        timer.finish();
        Ok((results, usage.gather_user_stats()))
    }

    pub async fn search_with_compiled_query(
        &self,
        index_id: IndexId,
        printable_index_name: IndexName,
        query: pb::searchlight::TextQuery,
        pending_updates: Vec<DocumentUpdate>,
        ts: RepeatableTimestamp,
    ) -> anyhow::Result<RevisionWithKeys> {
        let snapshot = self.snapshot(ts)?;
        let index = snapshot
            .index_registry
            .enabled_index_by_index_id(&index_id)
            .ok_or_else(|| anyhow::anyhow!("Missing index_id {:?}", index_id))?
            .clone();

        let search_snapshot = SearchIndexManagerSnapshot::new(
            snapshot.index_registry,
            snapshot.search_indexes,
            self.searcher.clone(),
            self.search_storage.clone(),
        );

        search_snapshot
            .search_with_compiled_query(&index, &printable_index_name, query, &pending_updates)
            .await
    }

    pub fn runtime(&self) -> &RT {
        &self.runtime
    }
}

/// Transaction statistics reported for a retried transaction
#[derive(Debug, PartialEq, Eq)]
pub struct OccRetryStats {
    /// Number of times the transaction was retried. 0 for a transaction that
    /// succeeded the first time.
    pub retries: u32,
    /// The duration of the successful transaction, not including commit
    pub duration: Duration,
    pub commit_duration: Duration,
    pub total_duration: Duration,
}

/// The read that conflicted as part of an OCC
#[derive(Debug, PartialEq, Eq)]
pub struct ConflictingRead {
    pub(crate) index: TabletIndexName,
    pub(crate) id: ResolvedDocumentId,
    pub(crate) stack_traces: Option<Vec<StackTrace>>,
}

fn occ_write_source_string(
    source: &Cow<'static, str>,
    document_id: String,
    is_same_write_source: bool,
) -> String {
    let preamble = if is_same_write_source {
        "Another call to this mutation".to_string()
    } else {
        format!("A call to \"{}\"", source)
    };
    format!(
        "{preamble} changed the document with ID \"{}\"",
        document_id
    )
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConflictingReadWithWriteSource {
    pub(crate) read: ConflictingRead,
    pub(crate) write_source: WriteSource,
}

impl ConflictingReadWithWriteSource {
    pub fn into_error(self, mapping: &TableMapping, current_writer: &WriteSource) -> anyhow::Error {
        let table_name = mapping.tablet_name(*self.read.index.table());

        let Ok(table_name) = table_name else {
            return anyhow::anyhow!(ErrorMetadata::user_occ(None, Option::<String>::None));
        };

        // We want to show the document's ID only if we know which mutation changed it,
        // so use it only if we have a write source.
        let occ_write_source = self.write_source.0.as_ref().map(|write_source| {
            occ_write_source_string(
                write_source,
                self.read.id.into(),
                *current_writer == self.write_source,
            )
        });

        if !table_name.is_system() {
            return anyhow::anyhow!(ErrorMetadata::user_occ(
                Some(table_name.into()),
                occ_write_source,
            ));
        }

        let msg = occ_write_source
            .map(|write_source| format!("{}.\n", write_source))
            .unwrap_or_default();
        let index = format!("{table_name}.{}", self.read.index.descriptor());
        let msg = format!("{msg}(conflicts with read of system table {index})");

        let formatted = if let Some(stack_traces) = self.read.stack_traces {
            format!(
                "{msg}. Displaying {}/{} stack traces of relevant reads. Increase \
                 NUM_READ_SET_STACKS for more:\n{}",
                cmp::min(*NUM_READ_SET_STACKS, stack_traces.len()),
                stack_traces.len(),
                stack_traces
                    .iter()
                    .take(*NUM_READ_SET_STACKS)
                    .join(&format!("\nRead of {index} occured at\n"))
            )
        } else {
            format!(
                "{msg}. Use RUST_BACKTRACE=1 READ_SET_CAPTURE_BACKTRACES=true to find trace of \
                 relevant reads"
            )
        };
        anyhow::anyhow!(formatted).context(ErrorMetadata::system_occ())
    }
}

pub fn unauthorized_error(op: &'static str) -> ErrorMetadata {
    ErrorMetadata::forbidden("Unauthorized", format!("Operation {op} not permitted"))
}
