// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! The types for the dataflow crate.
//!
//! These are extracted into their own crate so that crates that only depend
//! on the interface of the dataflow crate, and not its implementation, can
//! avoid the dependency, as the dataflow crate is very slow to compile.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};
use timely::progress::frontier::Antichain;

use mz_expr::{CollectionPlan, GlobalId, MirRelationExpr, MirScalarExpr, OptimizedMirRelationExpr};
use mz_repr::{Diff, RelationType, Row};

use crate::sources::persistence::SourcePersistDesc;
use crate::types::sinks::SinkDesc;
use crate::types::sources::SourceDesc;

/// The response from a `Peek`.
///
/// Note that each `Peek` expects to generate exactly one `PeekResponse`, i.e.
/// we expect a 1:1 contract between `Peek` and `PeekResponse`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PeekResponse {
    Rows(Vec<(Row, NonZeroUsize)>),
    Error(String),
    Canceled,
}

/// The response from a `Peek`, with row multiplicities represented in unary.
///
/// Note that each `Peek` expects to generate exactly one `PeekResponse`, i.e.
/// we expect a 1:1 contract between `Peek` and `PeekResponseUnary`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PeekResponseUnary {
    Rows(Vec<Row>),
    Error(String),
    Canceled,
}

impl PeekResponse {
    pub fn unwrap_rows(self) -> Vec<(Row, NonZeroUsize)> {
        match self {
            PeekResponse::Rows(rows) => rows,
            PeekResponse::Error(_) | PeekResponse::Canceled => {
                panic!("PeekResponse::unwrap_rows called on {:?}", self)
            }
        }
    }
}

/// Various responses that can be communicated about the progress of a TAIL command.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum TailResponse<T = mz_repr::Timestamp> {
    /// A batch of updates over a non-empty interval of time.
    Batch(TailBatch<T>),
    /// The TAIL dataflow was dropped, leaving updates from this frontier onward unspecified.
    DroppedAt(Antichain<T>),
}

/// A batch of updates for the interval `[lower, upper)`.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct TailBatch<T> {
    /// The lower frontier of the batch of updates.
    pub lower: Antichain<T>,
    /// The upper frontier of the batch of updates.
    pub upper: Antichain<T>,
    /// All updates greater than `lower` and not greater than `upper`.
    pub updates: Vec<(T, Row, Diff)>,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
/// A batch of updates to be fed to a local input
pub struct Update<T = mz_repr::Timestamp> {
    pub row: Row,
    pub timestamp: T,
    pub diff: Diff,
}

/// A commonly used name for dataflows contain MIR expressions.
pub type DataflowDesc = DataflowDescription<OptimizedMirRelationExpr>;

/// An association of a global identifier to an expression.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildDesc<P> {
    pub id: GlobalId,
    pub plan: P,
}

/// A description of an instantation of a source.
///
/// This includes a description of the source, but additionally any
/// context-dependent options like the ability to apply filtering and
/// projection to the records as they emerge.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceInstanceDesc<T = mz_repr::Timestamp> {
    /// A description of the source to construct.
    pub description: crate::types::sources::SourceDesc,
    /// Arguments for this instantiation of the source.
    pub arguments: SourceInstanceArguments<T>,
}

/// Per-source construction arguments.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceInstanceArguments<T = mz_repr::Timestamp> {
    /// Optional linear operators that can be applied record-by-record.
    pub operators: Option<crate::LinearOperator>,
    /// A description of how to persist the source.
    pub persist: Option<crate::sources::persistence::SourcePersistDesc<T>>,
}

/// Type alias for source subscriptions, (dataflow_id, source_id).
pub type SourceInstanceId = (uuid::Uuid, mz_expr::GlobalId);

/// A formed request for source instantiation.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SourceInstanceRequest<T = mz_repr::Timestamp> {
    /// The source's own identifier.
    pub source_id: mz_expr::GlobalId,
    /// A dataflow identifier that should be unique across dataflows.
    pub dataflow_id: uuid::Uuid,
    /// Arguments to the source instantiation.
    pub arguments: SourceInstanceArguments<T>,
    /// Frontier beyond which updates must be correct.
    pub as_of: Antichain<T>,
}

impl<T> SourceInstanceRequest<T> {
    /// Source identifier uniquely identifing this instantation.
    pub fn unique_id(&self) -> SourceInstanceId {
        (self.dataflow_id, self.source_id)
    }
}

/// A description of a dataflow to construct and results to surface.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct DataflowDescription<P, T = mz_repr::Timestamp> {
    /// Sources instantiations made available to the dataflow.
    pub source_imports: BTreeMap<GlobalId, SourceInstanceDesc<T>>,
    /// Indexes made available to the dataflow.
    pub index_imports: BTreeMap<GlobalId, (IndexDesc, RelationType)>,
    /// Views and indexes to be built and stored in the local context.
    /// Objects must be built in the specific order, as there may be
    /// dependencies of later objects on prior identifiers.
    pub objects_to_build: Vec<BuildDesc<P>>,
    /// Indexes to be made available to be shared with other dataflows
    /// (id of new index, description of index, relationtype of base source/view)
    pub index_exports: BTreeMap<GlobalId, (IndexDesc, RelationType)>,
    /// sinks to be created
    /// (id of new sink, description of sink)
    pub sink_exports: BTreeMap<GlobalId, crate::types::sinks::SinkDesc<T>>,
    /// An optional frontier to which inputs should be advanced.
    ///
    /// If this is set, it should override the default setting determined by
    /// the upper bound of `since` frontiers contributing to the dataflow.
    /// It is an error for this to be set to a frontier not beyond that default.
    pub as_of: Option<Antichain<T>>,
    /// Human readable name
    pub debug_name: String,
    /// Unique ID of the dataflow
    pub id: uuid::Uuid,
}

impl<T> DataflowDescription<OptimizedMirRelationExpr, T> {
    /// Creates a new dataflow description with a human-readable name.
    pub fn new(name: String) -> Self {
        Self {
            source_imports: Default::default(),
            index_imports: Default::default(),
            objects_to_build: Vec::new(),
            index_exports: Default::default(),
            sink_exports: Default::default(),
            as_of: Default::default(),
            debug_name: name,
            id: uuid::Uuid::new_v4(),
        }
    }

    /// Imports a previously exported index.
    ///
    /// This method makes available an index previously exported as `id`, identified
    /// to the query by `description` (which names the view the index arranges, and
    /// the keys by which it is arranged).
    ///
    /// The `requesting_view` argument is currently necessary to correctly track the
    /// dependencies of views on indexes.
    pub fn import_index(&mut self, id: GlobalId, description: IndexDesc, typ: RelationType) {
        self.index_imports.insert(id, (description, typ));
    }

    /// Imports a source and makes it available as `id`.
    pub fn import_source(
        &mut self,
        id: GlobalId,
        description: SourceDesc,
        persist: Option<SourcePersistDesc<T>>,
    ) {
        // Import the source with no linear operators applied to it.
        // They may be populated by whole-dataflow optimization.
        self.source_imports.insert(
            id,
            SourceInstanceDesc {
                description,
                arguments: SourceInstanceArguments {
                    operators: None,
                    persist,
                },
            },
        );
    }

    /// Binds to `id` the relation expression `plan`.
    pub fn insert_plan(&mut self, id: GlobalId, plan: OptimizedMirRelationExpr) {
        self.objects_to_build.push(BuildDesc { id, plan });
    }

    /// Exports as `id` an index on `on_id`.
    ///
    /// Future uses of `import_index` in other dataflow descriptions may use `id`,
    /// as long as this dataflow has not been terminated in the meantime.
    pub fn export_index(&mut self, id: GlobalId, description: IndexDesc, on_type: RelationType) {
        // We first create a "view" named `id` that ensures that the
        // data are correctly arranged and available for export.
        self.insert_plan(
            id,
            OptimizedMirRelationExpr::declare_optimized(MirRelationExpr::ArrangeBy {
                input: Box::new(MirRelationExpr::global_get(
                    description.on_id,
                    on_type.clone(),
                )),
                keys: vec![description.key.clone()],
            }),
        );
        self.index_exports.insert(id, (description, on_type));
    }

    /// Exports as `id` a sink described by `description`.
    pub fn export_sink(&mut self, id: GlobalId, description: SinkDesc<T>) {
        self.sink_exports.insert(id, description);
    }

    /// Returns true iff `id` is already imported.
    pub fn is_imported(&self, id: &GlobalId) -> bool {
        self.objects_to_build.iter().any(|bd| &bd.id == id)
            || self.source_imports.keys().any(|i| i == id)
    }

    /// Assigns the `as_of` frontier to the supplied argument.
    ///
    /// This method allows the dataflow to indicate a frontier up through
    /// which all times should be advanced. This can be done for at least
    /// two reasons: 1. correctness and 2. performance.
    ///
    /// Correctness may require an `as_of` to ensure that historical detail
    /// is consolidated at representative times that do not present specific
    /// detail that is not specifically correct. For example, updates may be
    /// compacted to times that are no longer the source times, but instead
    /// some byproduct of when compaction was executed; we should not present
    /// those specific times as meaningfully different from other equivalent
    /// times.
    ///
    /// Performance may benefit from an aggressive `as_of` as it reduces the
    /// number of distinct moments at which collections vary. Differential
    /// dataflow will refresh its outputs at each time its inputs change and
    /// to moderate that we can minimize the volume of distinct input times
    /// as much as possible.
    ///
    /// Generally, one should consider setting `as_of` at least to the `since`
    /// frontiers of contributing data sources and as aggressively as the
    /// computation permits.
    pub fn set_as_of(&mut self, as_of: Antichain<T>) {
        self.as_of = Some(as_of);
    }

    /// The number of columns associated with an identifier in the dataflow.
    pub fn arity_of(&self, id: &GlobalId) -> usize {
        for (source_id, source) in self.source_imports.iter() {
            if source_id == id {
                return source.description.desc.arity();
            }
        }
        for (desc, typ) in self.index_imports.values() {
            if &desc.on_id == id {
                return typ.arity();
            }
        }
        for desc in self.objects_to_build.iter() {
            if &desc.id == id {
                return desc.plan.arity();
            }
        }
        panic!("GlobalId {} not found in DataflowDesc", id);
    }
}

impl<P, T> DataflowDescription<P, T>
where
    P: CollectionPlan,
{
    /// Identifiers of exported objects (indexes and sinks).
    pub fn export_ids(&self) -> impl Iterator<Item = GlobalId> + '_ {
        self.index_exports
            .keys()
            .chain(self.sink_exports.keys())
            .cloned()
    }

    /// Returns the description of the object to build with the specified
    /// identifier.
    ///
    /// # Panics
    ///
    /// Panics if `id` is not present in `objects_to_build` exactly once.
    pub fn build_desc(&self, id: GlobalId) -> &BuildDesc<P> {
        let mut builds = self.objects_to_build.iter().filter(|build| build.id == id);
        let build = builds
            .next()
            .unwrap_or_else(|| panic!("object to build id {id} unexpectedly missing"));
        assert!(builds.next().is_none());
        build
    }

    /// Computes the set of identifiers upon which the specified collection
    /// identifier depends.
    ///
    /// `id` must specify a valid object in `objects_to_build`.
    pub fn depends_on(&self, collection_id: GlobalId) -> BTreeSet<GlobalId> {
        let mut out = BTreeSet::new();
        self.depends_on_into(collection_id, &mut out);
        out
    }

    /// Like `depends_on`, but appends to an existing `BTreeSet`.
    pub fn depends_on_into(&self, collection_id: GlobalId, out: &mut BTreeSet<GlobalId>) {
        if self.source_imports.contains_key(&collection_id) {
            // The collection is provided by an imported source. Report the
            // dependency on the source.
            out.insert(collection_id);
            return;
        }

        // NOTE(benesch): we're not smart enough here to know *which* index
        // for the collection will be used, if one exists, so we have to report
        // the dependency on all of them.
        let mut found_index = false;
        for (index_id, (desc, _typ)) in &self.index_imports {
            if desc.on_id == collection_id {
                // The collection is provided by an imported index. Report the
                // dependency on the index.
                out.insert(*index_id);
                found_index = true;
            }
        }
        if found_index {
            return;
        }

        // The collection is not provided by a source or imported index.
        // It must be a collection whose plan we have handy. Recurse.
        let build = self.build_desc(collection_id);
        for id in build.plan.depends_on() {
            self.depends_on_into(id, out)
        }
    }

    /// Determine a unique id for this dataflow based on the indexes it exports.
    // TODO: The semantics of this function are only useful for command reconciliation at the moment.
    pub fn global_id(&self) -> Option<GlobalId> {
        // TODO: This could be implemented without heap allocation.
        let mut exports = self.export_ids().collect::<Vec<_>>();
        exports.sort_unstable();
        exports.dedup();
        if exports.len() == 1 {
            return exports.pop();
        } else {
            None
        }
    }
}

impl<P: PartialEq, T: timely::PartialOrder> DataflowDescription<P, T> {
    /// Determine if a dataflow description is compatible with this dataflow description.
    ///
    /// Compatible dataflows have equal exports, imports, and objects to build. The `as_of` of
    /// the receiver has to be less equal the `other` `as_of`.
    ///
    // TODO: The semantics of this function are only useful for command reconciliation at the moment.
    pub fn compatible_with(&self, other: &Self) -> bool {
        let equality = self.index_exports == other.index_exports
            && self.sink_exports == other.sink_exports
            && self.objects_to_build == other.objects_to_build
            && self.index_imports == other.index_imports
            && self.source_imports == other.source_imports;
        let partial = if let (Some(as_of), Some(other_as_of)) = (&self.as_of, &other.as_of) {
            timely::PartialOrder::less_equal(as_of, other_as_of)
        } else {
            false
        };
        equality && partial
    }
}

/// Types and traits related to the introduction of changing collections into `dataflow`.
pub mod sources {

    use std::collections::{BTreeMap, HashMap};
    use std::ops::Add;
    use std::path::PathBuf;
    use std::time::Duration;

    use anyhow::{anyhow, bail};
    use chrono::NaiveDateTime;
    use globset::Glob;
    use http::Uri;
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    use crate::gen::postgres_source::PostgresSourceDetails;
    use mz_kafka_util::KafkaAddrs;
    use mz_repr::{ColumnType, RelationDesc, RelationType, ScalarType};

    // Types and traits related to the *decoding* of data for sources.
    pub mod encoding {
        use anyhow::Context;
        use serde::{Deserialize, Serialize};

        use mz_interchange::{avro, protobuf};
        use mz_repr::{ColumnType, RelationDesc, ScalarType};

        /// A description of how to interpret data from various sources
        ///
        /// Almost all sources only present values as part of their records, but Kafka allows a key to be
        /// associated with each record, which has a possibly independent encoding.
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub enum SourceDataEncoding {
            Single(DataEncoding),
            KeyValue {
                key: DataEncoding,
                value: DataEncoding,
            },
        }

        /// A description of how each row should be decoded, from a string of bytes to a sequence of
        /// Differential updates.
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub enum DataEncoding {
            Avro(AvroEncoding),
            AvroOcf(AvroOcfEncoding),
            Protobuf(ProtobufEncoding),
            Csv(CsvEncoding),
            Regex(RegexEncoding),
            Postgres,
            Bytes,
            Text,
        }

        impl SourceDataEncoding {
            pub fn key_ref(&self) -> Option<&DataEncoding> {
                match self {
                    SourceDataEncoding::Single(_) => None,
                    SourceDataEncoding::KeyValue { key, .. } => Some(key),
                }
            }

            /// Return either the Single encoding if this was a `SourceDataEncoding::Single`, else return the value encoding
            pub fn value(self) -> DataEncoding {
                match self {
                    SourceDataEncoding::Single(encoding) => encoding,
                    SourceDataEncoding::KeyValue { value, .. } => value,
                }
            }

            pub fn value_ref(&self) -> &DataEncoding {
                match self {
                    SourceDataEncoding::Single(encoding) => encoding,
                    SourceDataEncoding::KeyValue { value, .. } => value,
                }
            }

            pub fn desc(&self) -> Result<(Option<RelationDesc>, RelationDesc), anyhow::Error> {
                Ok(match self {
                    SourceDataEncoding::Single(value) => (None, value.desc()?),
                    SourceDataEncoding::KeyValue { key, value } => {
                        (Some(key.desc()?), value.desc()?)
                    }
                })
            }
        }

        pub fn included_column_desc(included_columns: Vec<(&str, ColumnType)>) -> RelationDesc {
            let mut desc = RelationDesc::empty();
            for (name, ty) in included_columns {
                desc = desc.with_column(name, ty);
            }
            desc
        }

        impl DataEncoding {
            /// Computes the [`RelationDesc`] for the relation specified by this
            /// data encoding and envelope.
            ///
            /// If a key desc is provided it will be prepended to the returned desc
            fn desc(&self) -> Result<RelationDesc, anyhow::Error> {
                // Add columns for the data, based on the encoding format.
                Ok(match self {
                    DataEncoding::Bytes => {
                        RelationDesc::empty().with_column("data", ScalarType::Bytes.nullable(false))
                    }
                    DataEncoding::AvroOcf(AvroOcfEncoding {
                        reader_schema: schema,
                        ..
                    })
                    | DataEncoding::Avro(AvroEncoding { schema, .. }) => {
                        let parsed_schema =
                            avro::parse_schema(schema).context("validating avro schema")?;
                        avro::schema_to_relationdesc(parsed_schema)
                            .context("validating avro schema")?
                    }
                    DataEncoding::Protobuf(ProtobufEncoding {
                        descriptors,
                        message_name,
                        confluent_wire_format: _,
                    }) => protobuf::DecodedDescriptors::from_bytes(
                        descriptors,
                        message_name.to_owned(),
                    )?
                    .columns()
                    .iter()
                    .fold(RelationDesc::empty(), |desc, (name, ty)| {
                        desc.with_column(name, ty.clone())
                    }),
                    DataEncoding::Regex(RegexEncoding { regex }) => regex
                        .capture_names()
                        .enumerate()
                        // The first capture is the entire matched string. This will
                        // often not be useful, so skip it. If people want it they can
                        // just surround their entire regex in an explicit capture
                        // group.
                        .skip(1)
                        .fold(RelationDesc::empty(), |desc, (i, name)| {
                            let name = match name {
                                None => format!("column{}", i),
                                Some(name) => name.to_owned(),
                            };
                            let ty = ScalarType::String.nullable(true);
                            desc.with_column(name, ty)
                        }),
                    DataEncoding::Csv(CsvEncoding { columns, .. }) => match columns {
                        ColumnSpec::Count(n) => {
                            (1..=*n).into_iter().fold(RelationDesc::empty(), |desc, i| {
                                desc.with_column(
                                    format!("column{}", i),
                                    ScalarType::String.nullable(false),
                                )
                            })
                        }
                        ColumnSpec::Header { names } => names
                            .iter()
                            .map(|s| &**s)
                            .fold(RelationDesc::empty(), |desc, name| {
                                desc.with_column(name, ScalarType::String.nullable(false))
                            }),
                    },
                    DataEncoding::Text => RelationDesc::empty()
                        .with_column("text", ScalarType::String.nullable(false)),
                    DataEncoding::Postgres => RelationDesc::empty()
                        .with_column("oid", ScalarType::Int32.nullable(false))
                        .with_column(
                            "row_data",
                            ScalarType::List {
                                element_type: Box::new(ScalarType::String),
                                custom_oid: None,
                            }
                            .nullable(false),
                        ),
                })
            }

            pub fn op_name(&self) -> &'static str {
                match self {
                    DataEncoding::Bytes => "Bytes",
                    DataEncoding::AvroOcf { .. } => "AvroOcf",
                    DataEncoding::Avro(_) => "Avro",
                    DataEncoding::Protobuf(_) => "Protobuf",
                    DataEncoding::Regex { .. } => "Regex",
                    DataEncoding::Csv(_) => "Csv",
                    DataEncoding::Text => "Text",
                    DataEncoding::Postgres => "Postgres",
                }
            }
        }

        /// Encoding in Avro format.
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub struct AvroEncoding {
            pub schema: String,
            pub schema_registry_config: Option<mz_ccsr::ClientConfig>,
            pub confluent_wire_format: bool,
        }

        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub struct AvroOcfEncoding {
            pub reader_schema: String,
        }

        /// Encoding in Protobuf format.
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct ProtobufEncoding {
            pub descriptors: Vec<u8>,
            pub message_name: String,
            pub confluent_wire_format: bool,
        }

        /// Arguments necessary to define how to decode from CSV format
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub struct CsvEncoding {
            pub columns: ColumnSpec,
            pub delimiter: u8,
        }

        /// Determines the RelationDesc and decoding of CSV objects
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub enum ColumnSpec {
            /// The first row is not a header row, and all columns get default names like `columnN`.
            Count(usize),
            /// The first row is a header row and therefore does become data
            ///
            /// Each of the values in `names` becomes the default name of a column in the dataflow.
            Header { names: Vec<String> },
        }

        impl ColumnSpec {
            /// The number of columns described by the column spec.
            pub fn arity(&self) -> usize {
                match self {
                    ColumnSpec::Count(n) => *n,
                    ColumnSpec::Header { names } => names.len(),
                }
            }

            pub fn into_header_names(self) -> Option<Vec<String>> {
                match self {
                    ColumnSpec::Count(_) => None,
                    ColumnSpec::Header { names } => Some(names),
                }
            }
        }

        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
        pub struct RegexEncoding {
            pub regex: mz_repr::adt::regex::Regex,
        }
    }

    pub mod persistence {
        use serde::{Deserialize, Serialize};

        use mz_expr::PartitionId;

        /// The details needed to make a source that uses an external [`super::SourceConnector`] persistent.
        #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
        pub struct SourcePersistDesc<T = mz_repr::Timestamp> {
            /// The _current_ upper seal timestamp of all involved streams.
            ///
            /// NOTE: This timestamp is determined when the coordinator starts up or when the source is
            /// initially created. When a source is actively writing to this stream, the seal timestamp
            /// will progress beyond this timestamp.
            ///
            /// This is okay for now because we only want to allow one source instantiation for persistent
            /// sources, meaning the flow is usually this:
            ///
            ///  1. coordinator determines seal timestamp
            ///  2. seal timestamps for a source are sent to dataflow when rendering a source
            ///  3. coordinator (or anyone) never looks at this timestamp again.
            ///
            /// And when we restart, we start from step 1., at which time we are guaranteed not to have a
            /// source running already.
            pub upper_seal_ts: T,

            /// The _current_ compaction frontier (aka _since_) of all involved streams.
            ///
            /// NOTE: This timestamp is determined when the coordinator starts up or when the source is
            /// initially created. When a source is actively writing to this stream and allowing
            /// compaction, this will progress beyond this timestamp.
            ///
            /// This is okay for now because we only want to allow one source instantiation for persistent
            /// sources, meaning the flow is usually this:
            ///
            ///  1. coordinator determines since timestamp
            ///  2. timestamps for a source are sent to dataflow when rendering a source
            ///  3. coordinator (or anyone) never looks at this timestamp again.
            ///
            /// And when we restart, we start from step 1., at which time we are guaranteed not to have a
            /// source running already.
            pub since_ts: T,

            /// Name of the primary persisted stream of this source. This is what a consumer of the
            /// persisted data would be interested in while the secondary stream(s) of the source are an
            /// internal implementation detail.
            pub primary_stream: String,

            /// Persisted stream of timestamp bindings.
            pub timestamp_bindings_stream: String,

            /// Any additional details that we need to make the envelope logic stateful.
            pub envelope_desc: EnvelopePersistDesc,
        }

        /// The persistence details we need for persisting a source envelopes data structures.
        ///
        /// This is a 1:1 mapping from envelope to `EnvelopePersistDesc`, as opposed to having a `None`
        /// that covers all envelopes that don't need additional data. Mostly, to just be explicit, but
        /// also because there is already a `NONE` envelope.
        ///
        /// Some envelopes will require additional streams, which should be listed in the variant.
        #[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
        pub enum EnvelopePersistDesc {
            Upsert,
            None,
        }

        /// Structure wrapping a timestamp update from a source
        /// If RT, contains a partition count
        /// which informs workers that messages with Offset on PartititionId will be timestamped
        /// with Timestamp.
        #[derive(Clone, Debug, Serialize, Deserialize)]
        pub enum TimestampSourceUpdate {
            /// Update for an RT source: contains a new partition to add to this source.
            RealTime(PartitionId),
        }
    }

    /// Universal language for describing message positions in Materialize, in a source independent
    /// way. Invidual sources like Kafka or File sources should explicitly implement their own offset
    /// type that converts to/From MzOffsets. A 0-MzOffset denotes an empty stream.
    #[derive(
        Copy, Clone, Default, Debug, PartialEq, PartialOrd, Eq, Ord, Hash, Serialize, Deserialize,
    )]
    pub struct MzOffset {
        pub offset: i64,
    }

    impl std::fmt::Display for MzOffset {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.offset)
        }
    }

    impl Add<i64> for MzOffset {
        type Output = MzOffset;

        fn add(self, x: i64) -> MzOffset {
            MzOffset {
                offset: self.offset + x,
            }
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    pub struct KafkaOffset {
        pub offset: i64,
    }

    /// Convert from KafkaOffset to MzOffset (1-indexed)
    impl From<KafkaOffset> for MzOffset {
        fn from(kafka_offset: KafkaOffset) -> Self {
            MzOffset {
                offset: kafka_offset.offset + 1,
            }
        }
    }

    /// Convert from MzOffset to Kafka::Offset as long as
    /// the offset is not negative
    impl Into<KafkaOffset> for MzOffset {
        fn into(self) -> KafkaOffset {
            KafkaOffset {
                offset: self.offset - 1,
            }
        }
    }

    /// Which piece of metadata a column corresponds to
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum IncludedColumnSource {
        /// The materialize-specific notion of "position"
        ///
        /// This is legacy, and should be removed when default metadata is no longer included
        DefaultPosition,
        Partition,
        Offset,
        Timestamp,
        Topic,
        Headers,
    }

    /// Whether and how to include the decoded key of a stream in dataflows
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum KeyEnvelope {
        /// Never include the key in the output row
        None,
        /// For composite key encodings, pull the fields from the encoding into columns.
        Flattened,
        /// Upsert is identical to Flattened but differs for non-avro sources, for which key names are overwritten.
        LegacyUpsert,
        /// Always use the given name for the key.
        ///
        /// * For a single-field key, this means that the column will get the given name.
        /// * For a multi-column key, the columns will get packed into a [`ScalarType::Record`], and
        ///   that Record will get the given name.
        Named(String),
    }

    /// A column that was created via an `INCLUDE` expression
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct IncludedColumnPos {
        pub name: String,
        pub pos: usize,
    }

    /// The meaning of the timestamp number produced by data sources. This type
    /// is not concerned with the source of the timestamp (like if the data came
    /// from a Debezium consistency topic or a CDCv2 stream), instead only what the
    /// timestamp number means.
    ///
    /// Some variants here have attached data used to differentiate incomparable
    /// instantiations. These attached data types should be expanded in the future
    /// if we need to tell apart more kinds of sources.
    #[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize, Deserialize, Hash)]
    pub enum Timeline {
        /// EpochMilliseconds means the timestamp is the number of milliseconds since
        /// the Unix epoch.
        EpochMilliseconds,
        /// External means the timestamp comes from an external data source and we
        /// don't know what the number means. The attached String is the source's name,
        /// which will result in different sources being incomparable.
        External(String),
        /// User means the user has manually specified a timeline. The attached
        /// String is specified by the user, allowing them to decide sources that are
        /// joinable.
        User(String),
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum SourceEnvelope {
        /// If present, include the key columns as an output column of the source with the given properties.
        None(KeyEnvelope),
        Debezium(DebeziumEnvelope),
        Upsert(UpsertEnvelope),
        CdcV2,
    }

    /// `UnplannedSourceEnvelope` is a `SourceEnvelope` missing some information. This information
    /// is obtained in `UnplannedSourceEnvelope::desc`, where
    /// `UnplannedSourceEnvelope::into_source_envelope`
    /// creates a full `SourceEnvelope`
    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum UnplannedSourceEnvelope {
        None(KeyEnvelope),
        Debezium(DebeziumEnvelope),
        Upsert(UpsertStyle),
        CdcV2,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct UpsertEnvelope {
        /// What style of Upsert we are using
        pub style: UpsertStyle,
        /// The indices of the keys in the full value row, used
        /// to deduplicate data in `upsert_core`
        pub key_indices: Vec<usize>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum UpsertStyle {
        /// `ENVELOPE UPSERT`, where the key shape depends on the independent
        /// `KeyEnvelope`
        Default(KeyEnvelope),
        /// `ENVELOPE DEBEZIUM UPSERT`
        Debezium { after_idx: usize },
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct DebeziumEnvelope {
        /// The column index containing the `before` row
        pub before_idx: usize,
        /// The column index containing the `after` row
        pub after_idx: usize,
        pub mode: DebeziumMode,
    }

    /// Ordered means we can trust Debezium high water marks
    ///
    /// In standard operation, Debezium should always emit messages in position order, but
    /// messages may be duplicated.
    ///
    /// For example, this is a legal stream of Debezium event positions:
    ///
    /// ```text
    /// 1 2 3 2
    /// ```
    ///
    /// Note that `2` appears twice, but the *first* time it appeared it appeared in order.
    /// Any position below the highest-ever seen position is guaranteed to be a duplicate,
    /// and can be ignored.
    ///
    /// Now consider this stream:
    ///
    /// ```text
    /// 1 3 2
    /// ```
    ///
    /// In this case, `2` is sent *out* of order, and if it is ignored we will miss important
    /// state.
    ///
    /// It is possible for users to do things with multiple databases and multiple Debezium
    /// instances pointing at the same Kafka topic that mean that the Debezium guarantees do
    /// not hold, in which case we are required to track individual messages, instead of just
    /// the highest-ever-seen message.
    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum DebeziumMode {
        /// Do not perform any deduplication
        None,
        /// We can trust high water mark
        Ordered(DebeziumDedupProjection),
        /// We need to store some piece of state for every message
        Full(DebeziumDedupProjection),
        FullInRange {
            projection: DebeziumDedupProjection,
            pad_start: Option<NaiveDateTime>,
            start: NaiveDateTime,
            end: NaiveDateTime,
        },
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct DebeziumDedupProjection {
        /// The column index containing the debezium source metadata
        pub source_idx: usize,
        /// The record index of the `source.snapshot` field
        pub snapshot_idx: usize,
        /// The upstream database specific fields
        pub source_projection: DebeziumSourceProjection,
        /// The column index containing the debezium transaction metadata
        pub transaction_idx: usize,
        /// The record index of the `transaction.total_order` field
        pub total_order_idx: usize,
    }

    /// Debezium generates records that contain metadata about the upstream database. The structure of
    /// this metadata depends on the type of connector used. This struct records the relevant indices
    /// in the record, calculated during planning, so that the dataflow operator can unpack the
    /// structure and extract the relevant information.
    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum DebeziumSourceProjection {
        MySql {
            file: usize,
            pos: usize,
            row: usize,
        },
        Postgres {
            sequence: Option<usize>,
            lsn: usize,
        },
        SqlServer {
            change_lsn: usize,
            event_serial_no: usize,
        },
    }

    /// Computes the indices of the value's relation description that appear in the key.
    ///
    /// Returns an error if it detects a common columns between the two relations that has the same
    /// name but a different type, if a key column is missing from the value, and if the key relation
    /// has a column with no name.
    fn match_key_indices(
        key_desc: &RelationDesc,
        value_desc: &RelationDesc,
    ) -> anyhow::Result<Vec<usize>> {
        let mut indices = Vec::new();
        for (name, key_type) in key_desc.iter() {
            let (index, value_type) = value_desc
                .get_by_name(name)
                .ok_or_else(|| anyhow!("Value schema missing primary key column: {}", name))?;

            if key_type == value_type {
                indices.push(index);
            } else {
                bail!(
                    "key and value column types do not match: key {:?} vs. value {:?}",
                    key_type,
                    value_type
                );
            }
        }
        Ok(indices)
    }

    impl UnplannedSourceEnvelope {
        /// Transforms an `UnplannedSourceEnvelope` into a `SourceEnvelope`
        ///
        /// Panics if the input envelope is `UnplannedSourceEnvelope::Upsert` and
        /// key is not passed as `Some`
        fn into_source_envelope(self, key: Option<Vec<usize>>) -> SourceEnvelope {
            match self {
                UnplannedSourceEnvelope::Upsert(upsert_style) => {
                    SourceEnvelope::Upsert(UpsertEnvelope {
                        style: upsert_style,
                        key_indices: key.expect("into_source_envelope to be passed correct parameters for UnplannedSourceEnvelope::Upsert"),
                    })
                },
                UnplannedSourceEnvelope::Debezium(inner) => {
                    SourceEnvelope::Debezium(inner)
                }
                UnplannedSourceEnvelope::None(inner) => SourceEnvelope::None(inner),
                UnplannedSourceEnvelope::CdcV2 => SourceEnvelope::CdcV2,
            }
        }

        /// Computes the output relation of this envelope when applied on top of the decoded key and
        /// value relation desc
        pub fn desc(
            self,
            key_desc: Option<RelationDesc>,
            value_desc: RelationDesc,
            metadata_desc: RelationDesc,
        ) -> anyhow::Result<(SourceEnvelope, RelationDesc)> {
            Ok(match &self {
                UnplannedSourceEnvelope::None(key_envelope)
                | UnplannedSourceEnvelope::Upsert(UpsertStyle::Default(key_envelope)) => {
                    let key_desc = match key_desc {
                        Some(desc) => desc,
                        None => {
                            return Ok((
                                self.into_source_envelope(None),
                                value_desc.concat(metadata_desc),
                            ))
                        }
                    };

                    let (keyed, key) = match key_envelope {
                        KeyEnvelope::None => (value_desc, None),
                        KeyEnvelope::Flattened => {
                            // Add the key columns as a key.
                            let key_indices: Vec<usize> = (0..key_desc.arity()).collect();
                            let key_desc = key_desc.with_key(key_indices.clone());
                            (key_desc.concat(value_desc), Some(key_indices))
                        }
                        KeyEnvelope::LegacyUpsert => {
                            let key_indices: Vec<usize> = (0..key_desc.arity()).collect();
                            let key_desc = key_desc.with_key(key_indices.clone());
                            let names = (0..key_desc.arity()).map(|i| format!("key{}", i));
                            // Rename key columns to "keyN"
                            (
                                key_desc.with_names(names).concat(value_desc),
                                Some(key_indices),
                            )
                        }
                        KeyEnvelope::Named(key_name) => {
                            let key_desc = {
                                // if the key has multiple objects, nest them as a record inside of a single name
                                if key_desc.arity() > 1 {
                                    let key_type = key_desc.typ();
                                    let key_as_record = RelationType::new(vec![ColumnType {
                                        nullable: false,
                                        scalar_type: ScalarType::Record {
                                            fields: key_desc
                                                .iter_names()
                                                .zip(key_type.column_types.iter())
                                                .map(|(name, ty)| (name.clone(), ty.clone()))
                                                .collect(),
                                            custom_oid: None,
                                            custom_name: None,
                                        },
                                    }]);

                                    RelationDesc::new(key_as_record, [key_name.to_string()])
                                } else {
                                    key_desc.with_names([key_name.to_string()])
                                }
                            };
                            // In all cases the first column is the key
                            (key_desc.with_key(vec![0]).concat(value_desc), Some(vec![0]))
                        }
                    };
                    (self.into_source_envelope(key), keyed.concat(metadata_desc))
                }
                UnplannedSourceEnvelope::Debezium(DebeziumEnvelope { after_idx, .. })
                | UnplannedSourceEnvelope::Upsert(UpsertStyle::Debezium { after_idx }) => {
                    match &value_desc.typ().column_types[*after_idx].scalar_type {
                        ScalarType::Record { fields, .. } => {
                            let mut desc = RelationDesc::from_names_and_types(fields.clone());
                            let key = key_desc.map(|k| match_key_indices(&k, &desc)).transpose()?;
                            if let Some(key) = key.clone() {
                                desc = desc.with_key(key);
                            }

                            let desc = match self {
                                UnplannedSourceEnvelope::Upsert(_) => desc.concat(metadata_desc),
                                _ => desc,
                            };

                            (self.into_source_envelope(key), desc)
                        }
                        ty => bail!(
                            "Incorrect type for Debezium value, expected Record, got {:?}",
                            ty
                        ),
                    }
                }
                UnplannedSourceEnvelope::CdcV2 => {
                    // the correct types

                    // CdcV2 row data are in a record in a record in a list
                    match &value_desc.typ().column_types[0].scalar_type {
                        ScalarType::List { element_type, .. } => match &**element_type {
                            ScalarType::Record { fields, .. } => {
                                // TODO maybe check this by name
                                match &fields[0].1.scalar_type {
                                    ScalarType::Record { fields, .. } => (
                                        self.into_source_envelope(None),
                                        RelationDesc::from_names_and_types(fields.clone()),
                                    ),
                                    ty => {
                                        bail!("Unepxected type for MATERIALIZE envelope: {:?}", ty)
                                    }
                                }
                            }
                            ty => bail!("Unepxected type for MATERIALIZE envelope: {:?}", ty),
                        },
                        ty => bail!("Unepxected type for MATERIALIZE envelope: {:?}", ty),
                    }
                }
            })
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KafkaSourceConnector {
        pub addrs: KafkaAddrs,
        pub topic: String,
        // Represents options specified by user when creating the source, e.g.
        // security settings.
        pub config_options: BTreeMap<String, String>,
        // Map from partition -> starting offset
        pub start_offsets: HashMap<i32, i64>,
        pub group_id_prefix: Option<String>,
        pub cluster_id: Uuid,
        /// If present, include the timestamp as an output column of the source with the given name
        pub include_timestamp: Option<IncludedColumnPos>,
        /// If present, include the partition as an output column of the source with the given name.
        pub include_partition: Option<IncludedColumnPos>,
        /// If present, include the topic as an output column of the source with the given name.
        pub include_topic: Option<IncludedColumnPos>,
        /// If present, include the offset as an output column of the source with the given name.
        pub include_offset: Option<IncludedColumnPos>,
        pub include_headers: Option<IncludedColumnPos>,
    }

    /// Legacy logic included something like an offset into almost data streams
    ///
    /// Eventually we will require `INCLUDE <metadata>` for everything.
    pub fn provide_default_metadata(
        envelope: &UnplannedSourceEnvelope,
        encoding: &encoding::DataEncoding,
    ) -> bool {
        let is_avro = matches!(encoding, encoding::DataEncoding::Avro(_));
        let is_stateless_dbz = match envelope {
            UnplannedSourceEnvelope::Debezium(_) => true,
            _ => false,
        };

        !is_avro && !is_stateless_dbz
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum Compression {
        Gzip,
        None,
    }

    /// A source of updates for a relational collection.
    ///
    /// A source contains enough information to instantiate a stream of changes,
    /// as well as related metadata about the columns, their types, and properties
    /// of the collection.
    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct SourceDesc {
        pub connector: SourceConnector,
        pub desc: RelationDesc,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum SourceConnector {
        External {
            connector: ExternalSourceConnector,
            encoding: encoding::SourceDataEncoding,
            envelope: SourceEnvelope,
            metadata_columns: Vec<IncludedColumnSource>,
            ts_frequency: Duration,
            timeline: Timeline,
        },

        /// A local "source" is either fed by a local input handle, or by reading from a
        /// `persisted_source()`. For non-persisted sources, values that are to be inserted
        /// are sent from the coordinator and pushed into the handle on a worker.
        ///
        /// For persisted sources, the coordinator only writes new values to a persistent
        /// stream. These values will then "show up" here because we read from the same
        /// persistent stream.
        // TODO: We could split this up into a `Local` source, that is only fed by a local handle and a
        // `LocalPersistenceSource` which is fed from a `persisted_source()`. But moving the
        // persist_name from `SourceDesc` to here is already a huge simplification/clarification. The
        // persist description on a `SourceDesc` is now purely used to signal that a source actively
        // persists, while a `LocalPersistenceSource` is a source that happens to read from persistence
        // but doesn't persist itself.
        //
        // That additional split seems like a bigger undertaking, though, because it also needs changes
        // to the coordinator. And I don't know if I want to invest too much time there when I don't
        // yet know how Tables will work in a post-ingestd world.
        Local {
            timeline: Timeline,
            persisted_name: Option<String>,
        },
    }

    impl SourceConnector {
        /// Returns `true` if this connector yields input data (including
        /// timestamps) that is stable across restarts. This is important for
        /// exactly-once Sinks that need to ensure that the same data is written,
        /// even when failures/restarts happen.
        pub fn yields_stable_input(&self) -> bool {
            if let SourceConnector::External { connector, .. } = self {
                // Conservatively, set all Kafka, File, or AvroOcf sources as having stable inputs because
                // we know they will be read in a known, repeatable offset order (modulo compaction for some Kafka sources).
                match connector {
                    ExternalSourceConnector::Kafka(_)
                    | ExternalSourceConnector::File(_)
                    | ExternalSourceConnector::AvroOcf(_) => true,
                    // Currently, the Kinesis connector assigns "offsets" by counting the message in the order it was received
                    // and this order is not replayable across different reads of the same Kinesis stream.
                    ExternalSourceConnector::Kinesis(_) => false,
                    _ => false,
                }
            } else {
                false
            }
        }

        pub fn name(&self) -> &'static str {
            match self {
                SourceConnector::External { connector, .. } => connector.name(),
                SourceConnector::Local { .. } => "local",
            }
        }

        pub fn timeline(&self) -> Timeline {
            match self {
                SourceConnector::External { timeline, .. } => timeline.clone(),
                SourceConnector::Local { timeline, .. } => timeline.clone(),
            }
        }
        pub fn requires_single_materialization(&self) -> bool {
            if let SourceConnector::External { connector, .. } = self {
                connector.requires_single_materialization()
            } else {
                false
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum ExternalSourceConnector {
        Kafka(KafkaSourceConnector),
        Kinesis(KinesisSourceConnector),
        File(FileSourceConnector),
        AvroOcf(FileSourceConnector),
        S3(S3SourceConnector),
        Postgres(PostgresSourceConnector),
        PubNub(PubNubSourceConnector),
    }

    impl ExternalSourceConnector {
        /// Returns the name and type of each additional metadata column that
        /// Materialize will automatically append to the source's inherent columns.
        ///
        /// Presently, each source type exposes precisely one metadata column that
        /// corresponds to some source-specific record counter. For example, file
        /// sources use a line number, while Kafka sources use a topic offset.
        ///
        /// The columns declared here must be kept in sync with the actual source
        /// implementations that produce these columns.
        pub fn metadata_columns(&self, include_defaults: bool) -> Vec<(&str, ColumnType)> {
            let mut columns = Vec::new();
            let default_col = |name| (name, ScalarType::Int64.nullable(false));
            match self {
                Self::Kafka(KafkaSourceConnector {
                    include_partition: part,
                    include_timestamp: time,
                    include_topic: topic,
                    include_offset: offset,
                    include_headers: headers,
                    ..
                }) => {
                    let mut items = BTreeMap::new();
                    // put the offset at the end if necessary
                    if include_defaults && offset.is_none() {
                        items.insert(4, default_col("mz_offset"));
                    }

                    for (include, ty) in [
                        (offset, ScalarType::Int64),
                        (part, ScalarType::Int32),
                        (time, ScalarType::Timestamp),
                        (topic, ScalarType::String),
                        (
                            headers,
                            ScalarType::List {
                                element_type: Box::new(ScalarType::Record {
                                    fields: vec![
                                        (
                                            "key".into(),
                                            ColumnType {
                                                nullable: false,
                                                scalar_type: ScalarType::String,
                                            },
                                        ),
                                        (
                                            "value".into(),
                                            ColumnType {
                                                nullable: false,
                                                scalar_type: ScalarType::Bytes,
                                            },
                                        ),
                                    ],
                                    custom_oid: None,
                                    custom_name: None,
                                }),
                                custom_oid: None,
                            },
                        ),
                    ] {
                        if let Some(include) = include {
                            items.insert(include.pos + 1, (&include.name, ty.nullable(false)));
                        }
                    }

                    items.into_values().collect()
                }
                Self::File(_) => {
                    if include_defaults {
                        columns.push(default_col("mz_line_no"));
                    }
                    columns
                }
                Self::Kinesis(_) => {
                    if include_defaults {
                        columns.push(default_col("mz_offset"))
                    };
                    columns
                }
                Self::AvroOcf(_) => {
                    if include_defaults {
                        columns.push(default_col("mz_obj_no"))
                    };
                    columns
                }
                // TODO: should we include object key and possibly object-internal offset here?
                Self::S3(_) => {
                    if include_defaults {
                        columns.push(default_col("mz_record"))
                    };
                    columns
                }
                Self::Postgres(_) => vec![],
                Self::PubNub(_) => vec![],
            }
        }

        // TODO(bwm): get rid of this when we no longer have the notion of default metadata
        pub fn default_metadata_column_name(&self) -> Option<&str> {
            match self {
                ExternalSourceConnector::Kafka(_) => Some("mz_offset"),
                ExternalSourceConnector::Kinesis(_) => Some("mz_offset"),
                ExternalSourceConnector::File(_) => Some("mz_line_no"),
                ExternalSourceConnector::AvroOcf(_) => Some("mz_obj_no"),
                ExternalSourceConnector::S3(_) => Some("mz_record"),
                ExternalSourceConnector::Postgres(_) => None,
                ExternalSourceConnector::PubNub(_) => None,
            }
        }

        pub fn metadata_column_types(&self, include_defaults: bool) -> Vec<IncludedColumnSource> {
            match self {
                ExternalSourceConnector::Kafka(KafkaSourceConnector {
                    include_partition: part,
                    include_timestamp: time,
                    include_topic: topic,
                    include_offset: offset,
                    include_headers: headers,
                    ..
                }) => {
                    // create a sorted list of column types based on the order they were declared in sql
                    // TODO: should key be included in the sorted list? Breaking change, and it's
                    // already special (it commonly multiple columns embedded in it).
                    let mut items = BTreeMap::new();
                    if include_defaults {
                        items.insert(4, IncludedColumnSource::DefaultPosition);
                    }
                    for (include, ty) in [
                        (offset, IncludedColumnSource::Offset),
                        (part, IncludedColumnSource::Partition),
                        (time, IncludedColumnSource::Timestamp),
                        (topic, IncludedColumnSource::Topic),
                        (headers, IncludedColumnSource::Headers),
                    ] {
                        if let Some(include) = include {
                            items.insert(include.pos, ty);
                        }
                    }

                    items.into_values().collect()
                }

                ExternalSourceConnector::Kinesis(_)
                | ExternalSourceConnector::File(_)
                | ExternalSourceConnector::AvroOcf(_)
                | ExternalSourceConnector::S3(_) => {
                    if include_defaults {
                        vec![IncludedColumnSource::DefaultPosition]
                    } else {
                        Vec::new()
                    }
                }
                ExternalSourceConnector::Postgres(_) | ExternalSourceConnector::PubNub(_) => {
                    Vec::new()
                }
            }
        }

        /// Returns the name of the external source connector.
        pub fn name(&self) -> &'static str {
            match self {
                ExternalSourceConnector::Kafka(_) => "kafka",
                ExternalSourceConnector::Kinesis(_) => "kinesis",
                ExternalSourceConnector::File(_) => "file",
                ExternalSourceConnector::AvroOcf(_) => "avro-ocf",
                ExternalSourceConnector::S3(_) => "s3",
                ExternalSourceConnector::Postgres(_) => "postgres",
                ExternalSourceConnector::PubNub(_) => "pubnub",
            }
        }

        /// Optionally returns the name of the upstream resource this source corresponds to.
        /// (Currently only implemented for Kafka and Kinesis, to match old-style behavior
        ///  TODO: decide whether we want file paths and other upstream names to show up in metrics too.
        pub fn upstream_name(&self) -> Option<&str> {
            match self {
                ExternalSourceConnector::Kafka(KafkaSourceConnector { topic, .. }) => {
                    Some(topic.as_str())
                }
                ExternalSourceConnector::Kinesis(KinesisSourceConnector {
                    stream_name, ..
                }) => Some(stream_name.as_str()),
                ExternalSourceConnector::File(_) => None,
                ExternalSourceConnector::AvroOcf(_) => None,
                ExternalSourceConnector::S3(_) => None,
                ExternalSourceConnector::Postgres(_) => None,
                ExternalSourceConnector::PubNub(_) => None,
            }
        }

        pub fn requires_single_materialization(&self) -> bool {
            match self {
                ExternalSourceConnector::S3(c) => c.requires_single_materialization(),
                ExternalSourceConnector::Postgres(_) => true,

                ExternalSourceConnector::Kafka(_)
                | ExternalSourceConnector::Kinesis(_)
                | ExternalSourceConnector::File(_)
                | ExternalSourceConnector::AvroOcf(_)
                | ExternalSourceConnector::PubNub(_) => false,
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KinesisSourceConnector {
        pub stream_name: String,
        pub aws: AwsConfig,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct FileSourceConnector {
        pub path: PathBuf,
        pub tail: bool,
        pub compression: Compression,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct PostgresSourceConnector {
        pub conn: String,
        pub publication: String,
        pub slot_name: String,
        pub details: PostgresSourceDetails,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct PubNubSourceConnector {
        pub subscribe_key: String,
        pub channel: String,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct S3SourceConnector {
        pub key_sources: Vec<S3KeySource>,
        pub pattern: Option<Glob>,
        pub aws: AwsConfig,
        pub compression: Compression,
    }

    impl S3SourceConnector {
        fn requires_single_materialization(&self) -> bool {
            // SQS Notifications are not durable, multiple sources depending on them will get
            // non-intersecting subsets of objects to read
            self.key_sources
                .iter()
                .any(|s| matches!(s, S3KeySource::SqsNotifications { .. }))
        }
    }

    /// A Source of Object Key names, the argument of the `DISCOVER OBJECTS` clause
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub enum S3KeySource {
        /// Scan the S3 Bucket to discover keys to download
        Scan { bucket: String },
        /// Load object keys based on the contents of an S3 Notifications channel
        ///
        /// S3 notifications channels can be configured to go to SQS, which is the
        /// only target we currently support.
        SqsNotifications { queue: String },
    }

    /// A wrapper for [`Uri`] that implements [`Serialize`] and `Deserialize`.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct SerdeUri(#[serde(with = "http_serde::uri")] pub Uri);

    /// AWS configuration overrides for a source or sink.
    ///
    /// This is a distinct type from any of the configuration types built into the
    /// AWS SDK so that we can implement `Serialize` and `Deserialize`.
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct AwsConfig {
        /// AWS Credentials, or where to find them
        pub credentials: AwsCredentials,
        /// The AWS region to use.
        ///
        /// Uses the default region (looking at env vars, config files, etc) if not provided.
        pub region: Option<String>,
        /// The AWS role to assume.
        pub role: Option<AwsAssumeRole>,
        /// The custom AWS endpoint to use, if any.
        pub endpoint: Option<SerdeUri>,
    }

    /// AWS credentials for a source or sink.
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum AwsCredentials {
        /// Look for credentials using the [default credentials chain][credchain]
        ///
        /// [credchain]: aws_config::default_provider::credentials::DefaultCredentialsChain
        Default,
        /// Load credentials using the given named profile
        Profile { profile_name: String },
        /// Use the enclosed static credentials
        Static {
            access_key_id: String,
            secret_access_key: String,
            session_token: Option<String>,
        },
    }

    /// A role for Materialize to assume when performing AWS API calls.
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct AwsAssumeRole {
        /// The Amazon Resource Name of the role to assume.
        pub arn: String,
    }

    /// An external ID to use for all AWS AssumeRole operations.
    ///
    /// Note that it is critical for security that this ID can **not** be provided by users running
    /// in Materialize Cloud, it must be provided by Materialize. Currently this guarantee is
    /// satisfied by only making this accessible from the CLI, which users do not have access to.
    ///
    /// <https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_create_for-user_externalid.html>
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum AwsExternalId {
        NotProvided,
        ISwearThisCameFromACliArgOrEnvVariable(String),
    }

    impl Default for AwsExternalId {
        fn default() -> Self {
            AwsExternalId::NotProvided
        }
    }

    impl AwsExternalId {
        fn get(&self) -> Option<&str> {
            match self {
                AwsExternalId::NotProvided => None,
                AwsExternalId::ISwearThisCameFromACliArgOrEnvVariable(v) => Some(v),
            }
        }
    }

    impl AwsConfig {
        /// Loads the AWS SDK configuration object from the environment, then
        /// applies the overrides from this object.
        pub async fn load(&self, external_id: AwsExternalId) -> mz_aws_util::config::AwsConfig {
            use aws_config::default_provider::credentials::DefaultCredentialsChain;
            use aws_config::default_provider::region::DefaultRegionChain;
            use aws_config::sts::AssumeRoleProvider;
            use aws_smithy_http::endpoint::Endpoint;
            use aws_types::credentials::SharedCredentialsProvider;
            use aws_types::region::Region;

            let region = match &self.region {
                Some(region) => Some(Region::new(region.clone())),
                _ => {
                    let mut rc = DefaultRegionChain::builder();
                    if let AwsCredentials::Profile { profile_name } = &self.credentials {
                        rc = rc.profile_name(profile_name);
                    }
                    rc.build().region().await
                }
            };

            let mut cred_provider = match &self.credentials {
                AwsCredentials::Default => SharedCredentialsProvider::new(
                    DefaultCredentialsChain::builder()
                        .region(region.clone())
                        .build()
                        .await,
                ),
                AwsCredentials::Profile { profile_name } => SharedCredentialsProvider::new(
                    DefaultCredentialsChain::builder()
                        .profile_name(profile_name)
                        .region(region.clone())
                        .build()
                        .await,
                ),
                AwsCredentials::Static {
                    access_key_id,
                    secret_access_key,
                    session_token,
                } => SharedCredentialsProvider::new(aws_types::Credentials::from_keys(
                    access_key_id,
                    secret_access_key,
                    session_token.clone(),
                )),
            };

            if let Some(AwsAssumeRole { arn }) = &self.role {
                let mut role = AssumeRoleProvider::builder(arn).session_name("materialized");
                // This affects which region to perform STS on, not where
                // anything else happens.
                if let Some(region) = &region {
                    role = role.region(region.clone());
                }
                if let Some(external_id) = external_id.get() {
                    role = role.external_id(external_id);
                }
                cred_provider = SharedCredentialsProvider::new(role.build(cred_provider));
            }

            let loader = aws_config::from_env()
                .region(region)
                .credentials_provider(cred_provider);
            let mut config = mz_aws_util::config::AwsConfig::from_loader(loader).await;
            if let Some(endpoint) = &self.endpoint {
                config.set_endpoint(Endpoint::immutable(endpoint.0.clone()));
            }
            config
        }
    }
}

/// Types and traits related to reporting changing collections out of `dataflow`.
pub mod sinks {

    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use timely::progress::frontier::Antichain;
    use url::Url;

    use mz_expr::GlobalId;
    use mz_kafka_util::KafkaAddrs;
    use mz_repr::RelationDesc;

    /// A sink for updates to a relational collection.
    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct SinkDesc<T = mz_repr::Timestamp> {
        pub from: GlobalId,
        pub from_desc: RelationDesc,
        pub connector: SinkConnector,
        pub envelope: Option<SinkEnvelope>,
        pub as_of: SinkAsOf<T>,
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum SinkEnvelope {
        Debezium,
        Upsert,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct SinkAsOf<T = mz_repr::Timestamp> {
        pub frontier: Antichain<T>,
        pub strict: bool,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub enum SinkConnector {
        Kafka(KafkaSinkConnector),
        Tail(TailSinkConnector),
        AvroOcf(AvroOcfSinkConnector),
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KafkaSinkConsistencyConnector {
        pub topic: String,
        pub schema_id: i32,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KafkaSinkConnector {
        pub addrs: KafkaAddrs,
        pub topic: String,
        pub topic_prefix: String,
        pub key_desc_and_indices: Option<(RelationDesc, Vec<usize>)>,
        pub relation_key_indices: Option<Vec<usize>>,
        pub value_desc: RelationDesc,
        pub published_schema_info: Option<PublishedSchemaInfo>,
        pub consistency: Option<KafkaSinkConsistencyConnector>,
        pub exactly_once: bool,
        // Source dependencies for exactly-once sinks.
        pub transitive_source_dependencies: Vec<GlobalId>,
        // Maximum number of records the sink will attempt to send each time it is
        // invoked
        pub fuel: usize,
        pub config_options: BTreeMap<String, String>,
    }

    /// TODO(JLDLaughlin): Documentation.
    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct PublishedSchemaInfo {
        pub key_schema_id: Option<i32>,
        pub value_schema_id: i32,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct AvroOcfSinkConnector {
        pub value_desc: RelationDesc,
        pub path: PathBuf,
    }

    impl SinkConnector {
        /// Returns the name of the sink connector.
        pub fn name(&self) -> &'static str {
            match self {
                SinkConnector::AvroOcf(_) => "avro-ocf",
                SinkConnector::Kafka(_) => "kafka",
                SinkConnector::Tail(_) => "tail",
            }
        }

        /// Returns `true` if this sink requires sources to block timestamp binding
        /// compaction until all sinks that depend on a given source have finished
        /// writing out that timestamp.
        ///
        /// To achieve that, each sink will hold a `AntichainToken` for all of
        /// the sources it depends on, and will advance all of its source
        /// dependencies' compaction frontiers as it completes writes.
        ///
        /// Sinks that do need to hold back compaction need to insert an
        /// [`Antichain`] into `StorageState::sink_write_frontiers` that they update
        /// in order to advance the frontier that holds back upstream compaction
        /// of timestamp bindings.
        ///
        /// See also [`transitive_source_dependencies`](SinkConnector::transitive_source_dependencies).
        pub fn requires_source_compaction_holdback(&self) -> bool {
            match self {
                SinkConnector::Kafka(k) => k.exactly_once,
                SinkConnector::AvroOcf(_) => false,
                SinkConnector::Tail(_) => false,
            }
        }

        /// Returns the [`GlobalIds`](GlobalId) of the transitive sources of this
        /// sink.
        pub fn transitive_source_dependencies(&self) -> &[GlobalId] {
            match self {
                SinkConnector::Kafka(k) => &k.transitive_source_dependencies,
                SinkConnector::AvroOcf(_) => &[],
                SinkConnector::Tail(_) => &[],
            }
        }
    }

    #[derive(Default, Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
    pub struct TailSinkConnector {}

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum SinkConnectorBuilder {
        Kafka(KafkaSinkConnectorBuilder),
        AvroOcf(AvroOcfSinkConnectorBuilder),
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct AvroOcfSinkConnectorBuilder {
        pub path: PathBuf,
        pub file_name_suffix: String,
        pub value_desc: RelationDesc,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KafkaSinkConnectorBuilder {
        pub broker_addrs: KafkaAddrs,
        pub format: KafkaSinkFormat,
        /// A natural key of the sinked relation (view or source).
        pub relation_key_indices: Option<Vec<usize>>,
        /// The user-specified key for the sink.
        pub key_desc_and_indices: Option<(RelationDesc, Vec<usize>)>,
        pub value_desc: RelationDesc,
        pub topic_prefix: String,
        pub consistency_topic_prefix: Option<String>,
        pub consistency_format: Option<KafkaSinkFormat>,
        pub topic_suffix_nonce: String,
        pub partition_count: i32,
        pub replication_factor: i32,
        pub fuel: usize,
        pub config_options: BTreeMap<String, String>,
        // Forces the sink to always write to the same topic across restarts instead
        // of picking a new topic each time.
        pub reuse_topic: bool,
        // Source dependencies for exactly-once sinks.
        pub transitive_source_dependencies: Vec<GlobalId>,
        pub retention: KafkaSinkConnectorRetention,
    }

    #[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
    pub struct KafkaSinkConnectorRetention {
        pub duration: Option<Option<Duration>>,
        pub bytes: Option<i64>,
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
    pub enum KafkaSinkFormat {
        Avro {
            schema_registry_url: Url,
            key_schema: Option<String>,
            value_schema: String,
            ccsr_config: mz_ccsr::ClientConfig,
        },
        Json,
    }
}

/// An index storing processed updates so they can be queried
/// or reused in other computations
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct IndexDesc {
    /// Identity of the collection the index is on.
    pub on_id: GlobalId,
    /// Expressions to be arranged, in order of decreasing primacy.
    pub key: Vec<MirScalarExpr>,
}

// TODO: change contract to ensure that the operator is always applied to
// streams of rows
/// In-place restrictions that can be made to rows.
///
/// These fields indicate *optional* information that may applied to
/// streams of rows. Any row that does not satisfy all predicates may
/// be discarded, and any column not listed in the projection may be
/// replaced by a default value.
///
/// The intended order of operations is that the predicates are first
/// applied, and columns not in projection can then be overwritten with
/// default values. This allows the projection to avoid capturing columns
/// used by the predicates but not otherwise required.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Hash)]
pub struct LinearOperator {
    /// Rows that do not pass all predicates may be discarded.
    pub predicates: Vec<MirScalarExpr>,
    /// Columns not present in `projection` may be replaced with
    /// default values.
    pub projection: Vec<usize>,
}

impl LinearOperator {
    /// Reports whether this linear operator is trivial when applied to an
    /// input of the specified arity.
    pub fn is_trivial(&self, arity: usize) -> bool {
        self.predicates.is_empty() && self.projection.iter().copied().eq(0..arity)
    }
}
