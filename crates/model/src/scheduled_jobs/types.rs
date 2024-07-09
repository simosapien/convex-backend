use common::types::Timestamp;
#[cfg(any(test, feature = "testing"))]
use proptest::prelude::*;
use serde::{
    Deserialize,
    Serialize,
};
use serde_bytes::ByteBuf;
use serde_json::Value as JsonValue;
use sync_types::CanonicalizedUdfPath;
use value::{
    codegen_convex_serialization,
    ConvexArray,
};

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub struct ScheduledJob {
    pub udf_path: CanonicalizedUdfPath,
    #[cfg_attr(
        any(test, feature = "testing"),
        proptest(strategy = "proptest::arbitrary::any_with::<ConvexArray>((0..4).into())")
    )]
    pub udf_args: ConvexArray,

    pub state: ScheduledJobState,

    // next_ts is the timestamp when the job was scheduled, and should only be set on pending and
    // in-progress states. completed_ts is the timestamp when the job was completed, and should
    // only be set on success, failed, and canceled states. This allows us to use an index to find
    // jobs that still need to be processed and jobs that can be garbage collected without doing
    // multiple queries on different states and merging the results. original_scheduled_ts is the
    // timestamp when the job was scheduled, but does not get mutated as the job transitions
    // between states.
    pub next_ts: Option<Timestamp>,
    pub completed_ts: Option<Timestamp>,
    pub original_scheduled_ts: Timestamp,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SerializedScheduledJob {
    udf_path: String,
    udf_args: ByteBuf,
    state: SerializedScheduledJobState,
    next_ts: Option<i64>,
    completed_ts: Option<i64>,
    original_scheduled_ts: Option<i64>,
}

impl TryFrom<ScheduledJob> for SerializedScheduledJob {
    type Error = anyhow::Error;

    fn try_from(job: ScheduledJob) -> anyhow::Result<Self> {
        // Serialize the udf arguments as binary since we restrict what
        // field names can be used in a `Document`'s top-level object.
        let udf_args_json = JsonValue::from(job.udf_args);
        let udf_args_bytes = serde_json::to_vec(&udf_args_json)?;
        Ok(SerializedScheduledJob {
            udf_path: String::from(job.udf_path),
            udf_args: ByteBuf::from(udf_args_bytes),
            state: job.state.try_into()?,
            next_ts: job.next_ts.map(|ts| ts.into()),
            completed_ts: job.completed_ts.map(|ts| ts.into()),
            original_scheduled_ts: Some(job.original_scheduled_ts.into()),
        })
    }
}

impl TryFrom<SerializedScheduledJob> for ScheduledJob {
    type Error = anyhow::Error;

    fn try_from(value: SerializedScheduledJob) -> anyhow::Result<Self> {
        let udf_path = value.udf_path.parse()?;
        let udf_args_json: JsonValue = serde_json::from_slice(&value.udf_args)?;
        let udf_args = udf_args_json.try_into()?;
        let state = value.state.try_into()?;
        let next_ts = value.next_ts.map(|ts| ts.try_into()).transpose()?;
        let completed_ts = value.completed_ts.map(|ts| ts.try_into()).transpose()?;
        let original_scheduled_ts = match value.original_scheduled_ts {
            Some(ts) => ts.try_into()?,
            // We added original_scheduled_ts later, and thus there are some historical pending jobs
            // that don't have it set. In that case, fallback to next_ts, which is the original
            // schedule time.
            None => match next_ts {
                Some(next_ts) => next_ts,
                None => {
                    anyhow::bail!("Could not use next_ts as a fallback for original_scheduled_ts")
                },
            },
        };

        Ok(ScheduledJob {
            udf_path,
            udf_args,
            state,
            next_ts,
            completed_ts,
            original_scheduled_ts,
        })
    }
}

/// The state machine for scheduled jobs. Note that only actions go through the
/// InProgress state. Mutations jump straight from Pending to one of the
/// completion states.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum ScheduledJobState {
    /// Job has not started yet.
    Pending,
    /// Job has started running but is not completed yet. This state only
    /// applies to actions, and is used to make actions execute at most once.
    InProgress,

    /// Completion states
    /// Job finished running successully with no errors.
    Success,
    /// Job hit an error while running, which can either be a deterministic user
    /// JS error or an internal error such as a transient error when running
    /// actions or trying to run a function that is not a mutation or action.
    Failed(String),
    /// Job was canceled via the dashboard, ctx.scheduler.cancel, or recursively
    /// by a parent scheduled job that was canceled while in progress.
    Canceled,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum SerializedScheduledJobState {
    Pending,
    InProgress,
    Success,
    Failed { error: String },
    Canceled,
}

impl TryFrom<ScheduledJobState> for SerializedScheduledJobState {
    type Error = anyhow::Error;

    fn try_from(state: ScheduledJobState) -> anyhow::Result<Self> {
        match state {
            ScheduledJobState::Pending => Ok(SerializedScheduledJobState::Pending),
            ScheduledJobState::InProgress => Ok(SerializedScheduledJobState::InProgress),
            ScheduledJobState::Success => Ok(SerializedScheduledJobState::Success),
            ScheduledJobState::Failed(e) => Ok(SerializedScheduledJobState::Failed { error: e }),
            ScheduledJobState::Canceled => Ok(SerializedScheduledJobState::Canceled),
        }
    }
}

impl TryFrom<SerializedScheduledJobState> for ScheduledJobState {
    type Error = anyhow::Error;

    fn try_from(value: SerializedScheduledJobState) -> anyhow::Result<Self> {
        match value {
            SerializedScheduledJobState::Pending => Ok(ScheduledJobState::Pending),
            SerializedScheduledJobState::InProgress => Ok(ScheduledJobState::InProgress),
            SerializedScheduledJobState::Success => Ok(ScheduledJobState::Success),
            SerializedScheduledJobState::Failed { error } => Ok(ScheduledJobState::Failed(error)),
            SerializedScheduledJobState::Canceled => Ok(ScheduledJobState::Canceled),
        }
    }
}

codegen_convex_serialization!(ScheduledJob, SerializedScheduledJob);

mod state {
    use value::codegen_convex_serialization;

    use super::{
        ScheduledJobState,
        SerializedScheduledJobState,
    };

    codegen_convex_serialization!(ScheduledJobState, SerializedScheduledJobState);
}
