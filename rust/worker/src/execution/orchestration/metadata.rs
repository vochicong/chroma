use crate::errors::{ChromaError, ErrorCodes};
use crate::execution::data::data_chunk::Chunk;
use crate::execution::operator::{wrap, TaskResult};
use crate::execution::operators::count_records::{
    CountRecordsError, CountRecordsInput, CountRecordsOperator, CountRecordsOutput,
};
use crate::execution::operators::merge_metadata_results::{
    MergeMetadataResultsOperator, MergeMetadataResultsOperatorError,
    MergeMetadataResultsOperatorInput, MergeMetadataResultsOperatorOutput,
};
use crate::execution::operators::pull_log::{PullLogsInput, PullLogsOperator, PullLogsOutput};
use crate::index::metadata::types::MetadataIndexError;
use crate::log::log::PullLogsError;
use crate::segment::metadata_segment::MetadataSegmentReader;
use crate::sysdb::sysdb::{GetCollectionsError, GetSegmentsError};
use crate::system::{Component, ComponentContext, Handler};
use crate::types::{Collection, LogRecord, Metadata, SegmentType};
use crate::types::{Where, WhereDocument};
use crate::{
    blockstore::provider::BlockfileProvider,
    execution::operator::TaskMessage,
    log::log::Log,
    sysdb::sysdb::SysDb,
    system::{Receiver, System},
    types::Segment,
};
use async_trait::async_trait;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tracing::Span;
use uuid::Uuid;

#[derive(Debug)]
enum ExecutionState {
    Pending,
    PullLogs,
    Filter, // Filter logs and search metadata segment
    MergeResults,
}

// Returns the ids, metadata, and documents
type MetadataQueryOrchestratorResult =
    Result<(Vec<String>, Vec<Option<Metadata>>, Vec<Option<String>>), Box<dyn ChromaError>>;

#[derive(Debug)]
pub(crate) struct MetadataQueryOrchestrator {
    state: ExecutionState,
    // Component Execution
    system: System,
    // Query state
    metadata_segment_id: Uuid,
    query_ids: Option<Vec<String>>,
    // State fetched or created for query execution
    record_segment: Option<Segment>,
    collection: Option<Collection>,
    // State machine management
    merge_dependency_count: u32,
    // Services
    log: Box<dyn Log>,
    sysdb: Box<dyn SysDb>,
    dispatcher: Box<dyn Receiver<TaskMessage>>,
    blockfile_provider: BlockfileProvider,
    // Query params
    where_clause: Option<Where>,
    where_document_clause: Option<WhereDocument>,
    // Result channel
    result_channel: Option<tokio::sync::oneshot::Sender<MetadataQueryOrchestratorResult>>,
}

#[derive(Debug)]
pub(crate) struct CountQueryOrchestrator {
    // Component Execution
    system: System,
    // Query state
    metadata_segment_id: Uuid,
    // State fetched or created for query execution
    record_segment: Option<Segment>,
    collection: Option<Collection>,
    // Services
    log: Box<dyn Log>,
    sysdb: Box<dyn SysDb>,
    dispatcher: Box<dyn Receiver<TaskMessage>>,
    blockfile_provider: BlockfileProvider,
    // Result channel
    result_channel: Option<tokio::sync::oneshot::Sender<Result<usize, Box<dyn ChromaError>>>>,
}

#[derive(Error, Debug)]
enum MetadataSegmentQueryError {
    #[error("Blockfile metadata segment with id: {0} not found")]
    BlockfileMetadataSegmentNotFound(Uuid),
    #[error("Get segments error")]
    GetSegmentsError(#[from] GetSegmentsError),
    #[error("Record segment not found for collection: {0}")]
    RecordSegmentNotFound(Uuid),
    #[error("Metadata segment has no collection")]
    MetadataSegmentHasNoCollection,
    #[error("System Time Error")]
    SystemTimeError(#[from] std::time::SystemTimeError),
    #[error("Collection not found for id: {0}")]
    CollectionNotFound(Uuid),
    #[error("Get collection error")]
    GetCollectionError(#[from] GetCollectionsError),
}

impl ChromaError for MetadataSegmentQueryError {
    fn code(&self) -> ErrorCodes {
        match self {
            MetadataSegmentQueryError::BlockfileMetadataSegmentNotFound(_) => ErrorCodes::NotFound,
            MetadataSegmentQueryError::GetSegmentsError(e) => e.code(),
            MetadataSegmentQueryError::RecordSegmentNotFound(_) => ErrorCodes::NotFound,
            MetadataSegmentQueryError::MetadataSegmentHasNoCollection => {
                ErrorCodes::InvalidArgument
            }
            MetadataSegmentQueryError::SystemTimeError(_) => ErrorCodes::Internal,
            MetadataSegmentQueryError::CollectionNotFound(_) => ErrorCodes::NotFound,
            MetadataSegmentQueryError::GetCollectionError(e) => e.code(),
        }
    }
}

impl CountQueryOrchestrator {
    pub(crate) fn new(
        system: System,
        metadata_segment_id: &Uuid,
        log: Box<dyn Log>,
        sysdb: Box<dyn SysDb>,
        dispatcher: Box<dyn Receiver<TaskMessage>>,
        blockfile_provider: BlockfileProvider,
    ) -> Self {
        Self {
            system,
            metadata_segment_id: *metadata_segment_id,
            record_segment: None,
            collection: None,
            log,
            sysdb,
            dispatcher,
            blockfile_provider,
            result_channel: None,
        }
    }

    async fn start(&mut self, ctx: &ComponentContext<Self>) {
        println!("Starting Count Query Orchestrator");
        // Populate the orchestrator with the initial state - The Record Segment and the Collection
        let metdata_segment = self
            .get_metadata_segment_from_id(self.sysdb.clone(), &self.metadata_segment_id)
            .await;

        let metadata_segment = match metdata_segment {
            Ok(segment) => segment,
            Err(e) => {
                tracing::error!("Error getting metadata segment: {:?}", e);
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        let collection_id = match metadata_segment.collection {
            Some(collection_id) => collection_id,
            None => {
                tracing::error!("Metadata segment has no collection");
                self.terminate_with_error(
                    Box::new(MetadataSegmentQueryError::MetadataSegmentHasNoCollection),
                    ctx,
                );
                return;
            }
        };

        let record_segment = self
            .get_record_segment_from_collection_id(self.sysdb.clone(), &collection_id)
            .await;

        let record_segment = match record_segment {
            Ok(segment) => segment,
            Err(e) => {
                tracing::error!("Error getting record segment: {:?}", e);
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        let collection = match self
            .get_collection_from_id(self.sysdb.clone(), &collection_id, ctx)
            .await
        {
            Ok(collection) => collection,
            Err(e) => {
                tracing::error!("Error getting collection: {:?}", e);
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        self.record_segment = Some(record_segment);
        self.collection = Some(collection);
    }

    async fn pull_logs(&mut self, ctx: &ComponentContext<Self>) {
        println!("Count query orchestrator pulling logs");

        let operator = PullLogsOperator::new(self.log.clone());
        let end_timestamp = SystemTime::now().duration_since(UNIX_EPOCH);
        let end_timestamp = match end_timestamp {
            Ok(end_timestamp) => end_timestamp.as_nanos() as i64,
            Err(e) => {
                tracing::error!("Error getting system time: {:?}", e);
                self.terminate_with_error(
                    Box::new(MetadataSegmentQueryError::SystemTimeError(e)),
                    ctx,
                );
                return;
            }
        };

        let collection = self
            .collection
            .as_ref()
            .expect("Invariant violation. Collection is not set before pull logs state.");
        let input = PullLogsInput::new(
            collection.id,
            // The collection log position is inclusive, and we want to start from the next log.
            collection.log_position + 1,
            100,
            None,
            Some(end_timestamp),
        );

        let task = wrap(operator, input, ctx.sender.as_receiver());
        match self.dispatcher.send(task, Some(Span::current())).await {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implies the dispatcher was dropped somehow
                // and is likely fatal
                println!("Error sending Count Query task: {:?}", e);
            }
        }
    }

    async fn get_metadata_segment_from_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        metadata_segment_id: &Uuid,
    ) -> Result<Segment, Box<dyn ChromaError>> {
        let segments = sysdb
            .get_segments(Some(*metadata_segment_id), None, None, None)
            .await;
        let segment = match segments {
            Ok(segments) => {
                if segments.is_empty() {
                    return Err(Box::new(
                        MetadataSegmentQueryError::BlockfileMetadataSegmentNotFound(
                            *metadata_segment_id,
                        ),
                    ));
                }
                segments[0].clone()
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetSegmentsError(e)));
            }
        };

        if segment.r#type != SegmentType::BlockfileMetadata {
            return Err(Box::new(
                MetadataSegmentQueryError::BlockfileMetadataSegmentNotFound(*metadata_segment_id),
            ));
        }
        Ok(segment)
    }

    async fn get_record_segment_from_collection_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        collection_id: &Uuid,
    ) -> Result<Segment, Box<dyn ChromaError>> {
        let segments = sysdb
            .get_segments(
                None,
                Some(SegmentType::Record.into()),
                None,
                Some(*collection_id),
            )
            .await;

        match segments {
            Ok(segments) => {
                if segments.is_empty() {
                    return Err(Box::new(MetadataSegmentQueryError::RecordSegmentNotFound(
                        *collection_id,
                    )));
                }
                // Unwrap is safe as we know at least one segment exists from
                // the check above
                return Ok(segments.into_iter().next().unwrap());
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetSegmentsError(e)));
            }
        };
    }

    async fn get_collection_from_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        collection_id: &Uuid,
        ctx: &ComponentContext<Self>,
    ) -> Result<Collection, Box<dyn ChromaError>> {
        let collections = sysdb
            .get_collections(Some(*collection_id), None, None, None)
            .await;

        match collections {
            Ok(collections) => {
                if collections.is_empty() {
                    return Err(Box::new(MetadataSegmentQueryError::CollectionNotFound(
                        *collection_id,
                    )));
                }
                // Unwrap is safe as we know at least one collection exists from
                // the check above
                return Ok(collections.into_iter().next().unwrap());
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetCollectionError(e)));
            }
        };
    }

    fn terminate_with_error(&mut self, error: Box<dyn ChromaError>, ctx: &ComponentContext<Self>) {
        let result_channel = self
            .result_channel
            .take()
            .expect("Invariant violation. Result channel is not set.");
        match result_channel.send(Err(error)) {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implied the listener was dropped
                println!("[CountQueryOrchestrator] Result channel dropped before sending error");
            }
        }
        // Cancel the orchestrator so it stops processing
        ctx.cancellation_token.cancel();
    }

    ///  Run the orchestrator and return the result.
    ///  # Note
    ///  Use this over spawning the component directly. This method will start the component and
    ///  wait for it to finish before returning the result.
    pub(crate) async fn run(mut self) -> Result<usize, Box<dyn ChromaError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.result_channel = Some(tx);
        let mut handle = self.system.clone().start_component(self);
        let result = rx.await;
        handle.stop();
        result.unwrap()
    }
}

#[async_trait]
impl Component for CountQueryOrchestrator {
    fn get_name() -> &'static str {
        "Count Query Orchestrator"
    }

    fn queue_size(&self) -> usize {
        1000 // TODO: make this configurable
    }

    async fn on_start(&mut self, ctx: &crate::system::ComponentContext<Self>) -> () {
        self.start(ctx).await;
        self.pull_logs(ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<PullLogsOutput, PullLogsError>> for CountQueryOrchestrator {
    async fn handle(
        &mut self,
        message: TaskResult<PullLogsOutput, PullLogsError>,
        ctx: &ComponentContext<Self>,
    ) {
        let message = message.into_inner();
        match message {
            Ok(logs) => {
                let operator = CountRecordsOperator::new();
                let input = CountRecordsInput::new(
                    self.record_segment
                        .as_ref()
                        .expect("Expect segment")
                        .clone(),
                    self.blockfile_provider.clone(),
                    logs.logs(),
                );
                let msg = wrap(operator, input, ctx.sender.as_receiver());
                match self.dispatcher.send(msg, None).await {
                    Ok(_) => (),
                    Err(e) => {
                        // Log an error - this implies the dispatcher was dropped somehow
                        // and is likely fatal
                        println!("Error sending Count Query task: {:?}", e);
                    }
                }
            }
            Err(e) => {
                self.terminate_with_error(Box::new(e), ctx);
            }
        }
    }
}

#[async_trait]
impl Handler<TaskResult<CountRecordsOutput, CountRecordsError>> for CountQueryOrchestrator {
    async fn handle(
        &mut self,
        message: TaskResult<CountRecordsOutput, CountRecordsError>,
        ctx: &ComponentContext<Self>,
    ) {
        let message = message.into_inner();
        let msg = match message {
            Ok(m) => m,
            Err(e) => {
                return self.terminate_with_error(Box::new(e), ctx);
            }
        };
        let channel = self
            .result_channel
            .take()
            .expect("Expect channel to be present");
        match channel.send(Ok(msg.count)) {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implied the listener was dropped
                println!("[CountQueryOrchestrator] Result channel dropped before sending result");
            }
        }
    }
}

impl MetadataQueryOrchestrator {
    pub(crate) fn new(
        system: System,
        metadata_segment_id: &Uuid,
        query_ids: Option<Vec<String>>,
        log: Box<dyn Log>,
        sysdb: Box<dyn SysDb>,
        dispatcher: Box<dyn Receiver<TaskMessage>>,
        blockfile_provider: BlockfileProvider,
        where_clause: Option<Where>,
        where_document_clause: Option<WhereDocument>,
    ) -> Self {
        Self {
            state: ExecutionState::Pending,
            system,
            metadata_segment_id: *metadata_segment_id,
            query_ids,
            record_segment: None,
            collection: None,
            merge_dependency_count: 2,
            log,
            sysdb,
            dispatcher,
            blockfile_provider,
            where_clause,
            where_document_clause,
            result_channel: None,
        }
    }

    async fn start(&mut self, ctx: &ComponentContext<Self>) {
        println!("Starting Metadata Query Orchestrator");
        // Populate the orchestrator with the initial state - The Metadata Segment, The Record Segment and the Collection
        let metdata_segment = self
            .get_metadata_segment_from_id(self.sysdb.clone(), &self.metadata_segment_id)
            .await;

        let metadata_segment = match metdata_segment {
            Ok(segment) => segment,
            Err(e) => {
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        let collection_id = match metadata_segment.collection {
            Some(collection_id) => collection_id,
            None => {
                self.terminate_with_error(
                    Box::new(MetadataSegmentQueryError::MetadataSegmentHasNoCollection),
                    ctx,
                );
                return;
            }
        };

        let record_segment = self
            .get_record_segment_from_collection_id(self.sysdb.clone(), &collection_id)
            .await;

        let record_segment = match record_segment {
            Ok(segment) => segment,
            Err(e) => {
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        let collection = match self
            .get_collection_from_id(self.sysdb.clone(), &collection_id, ctx)
            .await
        {
            Ok(collection) => collection,
            Err(e) => {
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        self.record_segment = Some(record_segment);
        self.collection = Some(collection);
    }

    async fn pull_logs(&mut self, ctx: &ComponentContext<Self>) {
        println!("Pulling logs");
        self.state = ExecutionState::PullLogs;

        let operator = PullLogsOperator::new(self.log.clone());
        let end_timestamp = SystemTime::now().duration_since(UNIX_EPOCH);
        let end_timestamp = match end_timestamp {
            Ok(end_timestamp) => end_timestamp.as_nanos() as i64,
            Err(e) => {
                self.terminate_with_error(
                    Box::new(MetadataSegmentQueryError::SystemTimeError(e)),
                    ctx,
                );
                return;
            }
        };

        let collection = self
            .collection
            .as_ref()
            .expect("Invariant violation. Collection is not set before pull logs state.");
        let input = PullLogsInput::new(
            collection.id,
            // The collection log position is inclusive, and we want to start from the next log.
            collection.log_position + 1,
            100,
            None,
            Some(end_timestamp),
        );

        let task = wrap(operator, input, ctx.sender.as_receiver());
        match self.dispatcher.send(task, Some(Span::current())).await {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implies the dispatcher was dropped somehow
                // and is likely fatal
                println!("Error sending Metadata Query task: {:?}", e);
            }
        }
    }

    async fn filter(&mut self, mut logs: Chunk<LogRecord>, ctx: &ComponentContext<Self>) {
        println!("Filtering logs and searching metadata segment");
        self.state = ExecutionState::Filter;

        let metdata_segment = self
            .get_metadata_segment_from_id(self.sysdb.clone(), &self.metadata_segment_id)
            .await;
        let metadata_segment = match metdata_segment {
            Ok(segment) => segment,
            Err(e) => {
                tracing::error!("Error getting metadata segment: {:?}", e);
                self.terminate_with_error(e, ctx);
                return;
            }
        };

        // TODO create segment in this method instead of constructor
        let metadata_segment_reader =
            MetadataSegmentReader::from_segment(&metadata_segment, &self.blockfile_provider).await;

        let filtered_index_offset_ids = match metadata_segment_reader {
            Ok(reader) => {
                reader
                    .query(
                        self.where_clause.as_ref(),
                        self.where_document_clause.as_ref(),
                        Some(&vec![]),
                        0,
                        0,
                    )
                    .await
            }
            Err(e) => {
                tracing::error!("Error querying metadata segment: {:?}", e);
                self.terminate_with_error(Box::new(e), ctx);
                return;
            }
        };
        let filtered_index_offset_ids = match filtered_index_offset_ids {
            Ok(filtered_index_offset_ids) => {
                let filtered_index_offset_ids = filtered_index_offset_ids
                    .into_iter()
                    .map(|index| index as u32)
                    .collect();
                Some(filtered_index_offset_ids)
            }
            Err(_) => None,
        };

        if self.query_ids.is_some() {
            // Build a query_id set
            let query_id_set: HashSet<String> =
                self.query_ids.as_ref().unwrap().iter().cloned().collect();
            // The query ids that are not present in the log
            let mut remaining_query_ids = query_id_set.clone();

            // Build the list of ids in the log to indices in the chunk
            let mut new_visibility = Vec::new();
            for (log_entry, _) in logs.iter() {
                if query_id_set.contains(&log_entry.record.id) {
                    new_visibility.push(true);
                    remaining_query_ids.remove(&log_entry.record.id);
                } else {
                    new_visibility.push(false);
                }
            }
            logs.set_visibility(new_visibility);

            let remaining_query_ids = remaining_query_ids.into_iter().collect();
            self.merge_results(
                logs,
                Some(remaining_query_ids),
                filtered_index_offset_ids,
                ctx,
            )
            .await;
        } else {
            // No query ids to filter on
            self.merge_results(logs, None, filtered_index_offset_ids, ctx)
                .await;
        }
    }

    async fn merge_results(
        &mut self,
        logs: Chunk<LogRecord>,
        remaining_query_ids: Option<Vec<String>>,
        filtered_index_offset_ids: Option<Vec<u32>>,
        ctx: &ComponentContext<Self>,
    ) {
        println!("Merging metadata results");
        self.state = ExecutionState::MergeResults;

        let operator = MergeMetadataResultsOperator::new();
        let input = MergeMetadataResultsOperatorInput::new(
            logs,
            remaining_query_ids,
            filtered_index_offset_ids,
            self.record_segment
                .as_ref()
                .expect("Invariant violation. Record segment is not set.")
                .clone(),
            self.blockfile_provider.clone(),
        );

        let task = wrap(operator, input, ctx.sender.as_receiver());
        match self.dispatcher.send(task, Some(Span::current())).await {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implies the dispatcher was dropped somehow
                // and is likely fatal
                println!("Error sending Metadata Query task: {:?}", e);
            }
        }
    }

    async fn get_metadata_segment_from_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        metadata_segment_id: &Uuid,
    ) -> Result<Segment, Box<dyn ChromaError>> {
        let segments = sysdb
            .get_segments(Some(*metadata_segment_id), None, None, None)
            .await;
        let segment = match segments {
            Ok(segments) => {
                if segments.is_empty() {
                    return Err(Box::new(
                        MetadataSegmentQueryError::BlockfileMetadataSegmentNotFound(
                            *metadata_segment_id,
                        ),
                    ));
                }
                segments[0].clone()
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetSegmentsError(e)));
            }
        };

        if segment.r#type != SegmentType::BlockfileMetadata {
            return Err(Box::new(
                MetadataSegmentQueryError::BlockfileMetadataSegmentNotFound(*metadata_segment_id),
            ));
        }
        Ok(segment)
    }

    async fn get_record_segment_from_collection_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        collection_id: &Uuid,
    ) -> Result<Segment, Box<dyn ChromaError>> {
        let segments = sysdb
            .get_segments(
                None,
                Some(SegmentType::Record.into()),
                None,
                Some(*collection_id),
            )
            .await;

        match segments {
            Ok(segments) => {
                if segments.is_empty() {
                    return Err(Box::new(MetadataSegmentQueryError::RecordSegmentNotFound(
                        *collection_id,
                    )));
                }
                // Unwrap is safe as we know at least one segment exists from
                // the check above
                return Ok(segments.into_iter().next().unwrap());
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetSegmentsError(e)));
            }
        };
    }

    async fn get_collection_from_id(
        &self,
        mut sysdb: Box<dyn SysDb>,
        collection_id: &Uuid,
        ctx: &ComponentContext<Self>,
    ) -> Result<Collection, Box<dyn ChromaError>> {
        let collections = sysdb
            .get_collections(Some(*collection_id), None, None, None)
            .await;

        match collections {
            Ok(collections) => {
                if collections.is_empty() {
                    return Err(Box::new(MetadataSegmentQueryError::CollectionNotFound(
                        *collection_id,
                    )));
                }
                // Unwrap is safe as we know at least one collection exists from
                // the check above
                return Ok(collections.into_iter().next().unwrap());
            }
            Err(e) => {
                return Err(Box::new(MetadataSegmentQueryError::GetCollectionError(e)));
            }
        };
    }

    fn terminate_with_error(&mut self, error: Box<dyn ChromaError>, ctx: &ComponentContext<Self>) {
        let result_channel = self
            .result_channel
            .take()
            .expect("Invariant violation. Result channel is not set.");
        match result_channel.send(Err(error)) {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implied the listener was dropped
                println!("[MetadataQueryOrchestrator] Result channel dropped before sending error");
            }
        }
        // Cancel the orchestrator so it stops processing
        ctx.cancellation_token.cancel();
    }

    ///  Run the orchestrator and return the result.
    ///  # Note
    ///  Use this over spawning the component directly. This method will start the component and
    ///  wait for it to finish before returning the result.
    pub(crate) async fn run(mut self) -> MetadataQueryOrchestratorResult {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.result_channel = Some(tx);
        let mut handle = self.system.clone().start_component(self);
        let result = rx.await;
        handle.stop();
        result.unwrap()
    }
}

#[async_trait]
impl Component for MetadataQueryOrchestrator {
    fn get_name() -> &'static str {
        "Metadata Query Orchestrator"
    }

    fn queue_size(&self) -> usize {
        1000 // TODO: make this configurable
    }

    async fn on_start(&mut self, ctx: &crate::system::ComponentContext<Self>) -> () {
        self.start(ctx).await;
        self.pull_logs(ctx).await;
    }
}

#[async_trait]
impl Handler<TaskResult<PullLogsOutput, PullLogsError>> for MetadataQueryOrchestrator {
    async fn handle(
        &mut self,
        message: TaskResult<PullLogsOutput, PullLogsError>,
        ctx: &ComponentContext<Self>,
    ) {
        let message = message.into_inner();
        match message {
            Ok(logs) => {
                let logs = logs.logs();
                self.filter(logs, ctx).await;
            }
            Err(e) => {
                tracing::error!("Error pulling logs: {:?}", e);
                self.terminate_with_error(Box::new(e), ctx);
            }
        }
    }
}

#[async_trait]
impl Handler<TaskResult<MergeMetadataResultsOperatorOutput, MergeMetadataResultsOperatorError>>
    for MetadataQueryOrchestrator
{
    async fn handle(
        &mut self,
        message: TaskResult<MergeMetadataResultsOperatorOutput, MergeMetadataResultsOperatorError>,
        ctx: &ComponentContext<Self>,
    ) {
        let message = message.into_inner();
        let output = match message {
            Ok(output) => output,
            Err(e) => {
                tracing::error!("Error merging metadata results: {:?}", e);
                return self.terminate_with_error(Box::new(e), ctx);
            }
        };

        let result_channel = self
            .result_channel
            .take()
            .expect("Invariant violation. Result channel is not set.");

        let output = (output.ids, output.metadata, output.documents);
        println!("Merged metadata results: {:?}", output);

        match result_channel.send(Ok(output)) {
            Ok(_) => (),
            Err(e) => {
                // Log an error - this implied the listener was dropped
                println!(
                    "[MetadataQueryOrchestrator] Result channel dropped before sending result"
                );
            }
        }
    }
}
