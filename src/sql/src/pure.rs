// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! SQL purification.
//!
//! See the [crate-level documentation](crate) for details.

use std::collections::BTreeMap;
use std::iter;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, ensure, Context};
use aws_arn::ARN;
use csv::ReaderBuilder;
use mz_sql_parser::ast::KafkaSourceConnector;
use prost::Message;
use protobuf_native::compiler::{SourceTreeDescriptorDatabase, VirtualSourceTree};
use protobuf_native::MessageLite;
use reqwest::Url;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::task;
use uuid::Uuid;

use mz_ccsr::{Client, GetBySubjectError};
use mz_dataflow_types::postgres_source::PostgresSourceDetails;
use mz_dataflow_types::sources::{AwsConfig, AwsExternalId};
use mz_repr::strconv;

use crate::ast::{
    AvroSchema, CreateSourceConnector, CreateSourceFormat, CreateSourceStatement, CsrConnectorAvro,
    CsrConnectorProto, CsrSeed, CsrSeedCompiled, CsrSeedCompiledEncoding, CsrSeedCompiledOrLegacy,
    CsvColumns, DbzMode, Envelope, Format, Ident, ProtobufSchema, Raw, SqlOption, Value,
    WithOption, WithOptionValue,
};
use crate::kafka_util;
use crate::normalize;

/// Purifies a statement, removing any dependencies on external state.
///
/// See the section on [purification](crate#purification) in the crate
/// documentation for details.
///
/// Note that purification is asynchronous, and may take an unboundedly long
/// time to complete. As a result purification does *not* have access to a
/// [`SessionCatalog`](crate::catalog::SessionCatalog), as that would require
/// locking access to the catalog for an unbounded amount of time.
pub async fn purify_create_source(
    now: u64,
    aws_external_id: AwsExternalId,
    mut stmt: CreateSourceStatement<Raw>,
) -> Result<CreateSourceStatement<Raw>, anyhow::Error> {
    let CreateSourceStatement {
        connector,
        format,
        envelope,
        with_options,
        include_metadata: _,
        ..
    } = &mut stmt;

    let mut with_options_map = normalize::options(with_options);
    let mut config_options = BTreeMap::new();

    let mut file = None;
    match connector {
        CreateSourceConnector::Kafka(KafkaSourceConnector {
            connector: broker,
            topic,
            ..
        }) => {
            match broker {
                // Temporary until the rest of the connector plumbing is finished
                mz_sql_parser::ast::KafkaConnector::Reference { .. } => unreachable!(),
                mz_sql_parser::ast::KafkaConnector::Inline { broker } => {
                    if !broker.contains(':') {
                        *broker += ":9092";
                    }

                    // Verify that the provided security options are valid and then test them.
                    config_options = kafka_util::extract_config(&mut with_options_map)?;
                    let consumer = kafka_util::create_consumer(&broker, &topic, &config_options)
                        .await
                        .map_err(|e| {
                            anyhow!("Failed to create and connect Kafka consumer: {}", e)
                        })?;

                    // Translate `kafka_time_offset` to `start_offset`.
                    match kafka_util::lookup_start_offsets(
                        Arc::clone(&consumer),
                        &topic,
                        &with_options_map,
                        now,
                    )
                    .await?
                    {
                        Some(start_offsets) => {
                            // Drop `kafka_time_offset`
                            with_options.retain(|val| match val {
                                mz_sql_parser::ast::SqlOption::Value { name, .. } => {
                                    name.as_str() != "kafka_time_offset"
                                }
                                _ => true,
                            });

                            // Add `start_offset`
                            with_options.push(mz_sql_parser::ast::SqlOption::Value {
                                name: mz_sql_parser::ast::Ident::new("start_offset"),
                                value: mz_sql_parser::ast::Value::Array(
                                    start_offsets
                                        .iter()
                                        .map(|offset| Value::Number(offset.to_string()))
                                        .collect(),
                                ),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        CreateSourceConnector::AvroOcf { path, .. } => {
            let path = path.clone();
            task::block_in_place(|| {
                // mz_avro::Reader has no async equivalent, so we're stuck
                // using blocking calls here.
                let f = std::fs::File::open(path)?;
                let r = mz_avro::Reader::new(f)?;
                if !with_options_map.contains_key("reader_schema") {
                    let schema = serde_json::to_string(r.writer_schema()).unwrap();
                    with_options.push(mz_sql_parser::ast::SqlOption::Value {
                        name: mz_sql_parser::ast::Ident::new("reader_schema"),
                        value: mz_sql_parser::ast::Value::String(schema),
                    });
                }
                Ok::<_, anyhow::Error>(())
            })?;
        }
        // Report an error if a file cannot be opened, or if it is a directory.
        CreateSourceConnector::File { path, .. } => {
            let f = File::open(&path).await?;
            if f.metadata().await?.is_dir() {
                bail!("Expected a regular file, but {} is a directory.", path);
            }
            file = Some(f);
        }
        CreateSourceConnector::S3 { .. } => {
            let aws_config = normalize::aws_config(&mut with_options_map, None)?;
            validate_aws_credentials(&aws_config, aws_external_id).await?;
        }
        CreateSourceConnector::Kinesis { arn } => {
            let region = arn
                .parse::<ARN>()
                .context("Unable to parse provided ARN")?
                .region
                .ok_or_else(|| anyhow!("Provided ARN does not include an AWS region"))?;

            let aws_config = normalize::aws_config(&mut with_options_map, Some(region.into()))?;
            validate_aws_credentials(&aws_config, aws_external_id).await?;
        }
        CreateSourceConnector::Postgres {
            conn,
            publication,
            slot,
            details,
        } => {
            slot.get_or_insert_with(|| {
                format!(
                    "materialize_{}",
                    Uuid::new_v4().to_string().replace('-', "")
                )
            });

            // verify that we can connect upstream and snapshot publication metadata
            let tables = mz_postgres_util::publication_info(&conn, &publication).await?;

            let details_proto = PostgresSourceDetails {
                tables: tables.into_iter().map(|t| t.into()).collect(),
                slot: slot.clone().expect("slot must exist"),
            };
            *details = Some(hex::encode(details_proto.encode_to_vec()));
        }
        CreateSourceConnector::PubNub { .. } => (),
    }

    purify_source_format(
        format,
        connector,
        &envelope,
        file,
        &config_options,
        with_options,
    )
    .await?;

    Ok(stmt)
}

async fn purify_source_format(
    format: &mut CreateSourceFormat<Raw>,
    connector: &mut CreateSourceConnector,
    envelope: &Envelope,
    file: Option<File>,
    connector_options: &BTreeMap<String, String>,
    with_options: &Vec<SqlOption<Raw>>,
) -> Result<(), anyhow::Error> {
    if matches!(format, CreateSourceFormat::KeyValue { .. })
        && !matches!(connector, CreateSourceConnector::Kafka { .. })
    {
        bail!("Kafka sources are the only source type that can provide KEY/VALUE formats")
    }

    // For backwards compatibility, using ENVELOPE UPSERT with a bare FORMAT
    // BYTES or FORMAT TEXT uses the specified format for both the key and the
    // value.
    //
    // TODO(bwm): We should either make these semantics apply everywhere, or
    // deprecate this.
    match (&connector, &envelope, &*format) {
        (
            CreateSourceConnector::Kafka { .. },
            Envelope::Upsert,
            CreateSourceFormat::Bare(f @ Format::Bytes | f @ Format::Text),
        ) => {
            *format = CreateSourceFormat::KeyValue {
                key: f.clone(),
                value: f.clone(),
            };
        }
        _ => (),
    }

    match format {
        CreateSourceFormat::None => {}
        CreateSourceFormat::Bare(format) => {
            purify_source_format_single(
                format,
                connector,
                envelope,
                file,
                connector_options,
                with_options,
            )
            .await?;
        }

        CreateSourceFormat::KeyValue { key, value: val } => {
            ensure!(
                file.is_none(),
                anyhow!("[internal-error] File sources cannot be key-value sources")
            );

            purify_source_format_single(
                key,
                connector,
                envelope,
                None,
                connector_options,
                with_options,
            )
            .await?;
            purify_source_format_single(
                val,
                connector,
                envelope,
                None,
                connector_options,
                with_options,
            )
            .await?;
        }
    }
    Ok(())
}

async fn purify_source_format_single(
    format: &mut Format<Raw>,
    connector: &mut CreateSourceConnector,
    envelope: &Envelope,
    file: Option<File>,
    connector_options: &BTreeMap<String, String>,
    with_options: &Vec<SqlOption<Raw>>,
) -> Result<(), anyhow::Error> {
    match format {
        Format::Avro(schema) => match schema {
            AvroSchema::Csr { csr_connector } => {
                purify_csr_connector_avro(connector, csr_connector, envelope, connector_options)
                    .await?
            }
            AvroSchema::InlineSchema {
                schema: mz_sql_parser::ast::Schema::File(path),
                with_options,
            } => {
                let file_schema = tokio::fs::read_to_string(path).await?;
                // Explicitly inject `confluent_wire_format = true`, if unset.
                // This, in combination with the catalog migration that sets
                // this option to true for sources created before this option
                // existed, will make it easy to flip the default to `false`
                // in the future, if we like.
                if !with_options
                    .iter()
                    .any(|option| option.key.as_str() == "confluent_wire_format")
                {
                    with_options.push(WithOption {
                        key: Ident::new("confluent_wire_format"),
                        value: Some(WithOptionValue::Value(Value::Boolean(true))),
                    });
                }
                *schema = AvroSchema::InlineSchema {
                    schema: mz_sql_parser::ast::Schema::Inline(file_schema),
                    with_options: with_options.clone(),
                };
            }
            _ => {}
        },
        Format::Protobuf(schema) => match schema {
            ProtobufSchema::Csr { csr_connector } => {
                purify_csr_connector_proto(connector, csr_connector, envelope, with_options)
                    .await?;
            }
            ProtobufSchema::InlineSchema {
                message_name: _,
                schema,
            } => {
                if let mz_sql_parser::ast::Schema::File(path) = schema {
                    let descriptors = tokio::fs::read(path).await?;
                    let mut buf = String::new();
                    strconv::format_bytes(&mut buf, &descriptors);
                    *schema = mz_sql_parser::ast::Schema::Inline(buf);
                }
            }
        },
        Format::Csv {
            delimiter,
            ref mut columns,
        } => {
            purify_csv(file, connector, *delimiter, columns).await?;
        }
        Format::Bytes | Format::Regex(_) | Format::Json | Format::Text => (),
    }
    Ok(())
}

async fn purify_csr_connector_proto(
    connector: &mut CreateSourceConnector,
    csr_connector: &mut CsrConnectorProto<Raw>,
    envelope: &Envelope,
    with_options: &Vec<SqlOption<Raw>>,
) -> Result<(), anyhow::Error> {
    let topic = if let CreateSourceConnector::Kafka(KafkaSourceConnector { topic, .. }) = connector
    {
        topic
    } else {
        bail!("Confluent Schema Registry is only supported with Kafka sources")
    };

    let CsrConnectorProto {
        url,
        seed,
        with_options: ccsr_options,
    } = csr_connector;
    match seed {
        None => {
            let url: Url = url.parse()?;
            let kafka_options = kafka_util::extract_config(&mut normalize::options(with_options))?;
            let ccsr_config = kafka_util::generate_ccsr_client_config(
                url,
                &kafka_options,
                &mut normalize::options(&ccsr_options),
            )?;

            let value =
                compile_proto(&format!("{}-value", topic), ccsr_config.clone().build()?).await?;
            let key = compile_proto(&format!("{}-key", topic), ccsr_config.build()?)
                .await
                .ok();

            if matches!(envelope, Envelope::Debezium(DbzMode::Upsert)) && key.is_none() {
                bail!("Key schema is required for ENVELOPE DEBEZIUM UPSERT");
            }

            *seed = Some(CsrSeedCompiledOrLegacy::Compiled(CsrSeedCompiled {
                value,
                key,
            }));
        }
        Some(CsrSeedCompiledOrLegacy::Compiled(..)) => (),
        Some(CsrSeedCompiledOrLegacy::Legacy(..)) => {
            unreachable!("Should not be able to purify CsrCeedCompiledOrLegacy::Legacy")
        }
    }

    Ok(())
}

async fn purify_csr_connector_avro(
    connector: &mut CreateSourceConnector,
    csr_connector: &mut CsrConnectorAvro<Raw>,
    envelope: &Envelope,
    connector_options: &BTreeMap<String, String>,
) -> Result<(), anyhow::Error> {
    let topic = if let CreateSourceConnector::Kafka(KafkaSourceConnector { topic, .. }) = connector
    {
        topic
    } else {
        bail!("Confluent Schema Registry is only supported with Kafka sources")
    };

    let CsrConnectorAvro {
        url,
        seed,
        with_options: ccsr_options,
    } = csr_connector;
    if seed.is_none() {
        let url = url.parse()?;

        let ccsr_config = task::block_in_place(|| {
            kafka_util::generate_ccsr_client_config(
                url,
                &connector_options,
                &mut normalize::options(ccsr_options),
            )
        })?;

        let Schema {
            key_schema,
            value_schema,
            ..
        } = get_remote_csr_schema(ccsr_config, topic.clone()).await?;
        if matches!(envelope, Envelope::Debezium(DbzMode::Upsert)) && key_schema.is_none() {
            bail!("Key schema is required for ENVELOPE DEBEZIUM UPSERT");
        }

        *seed = Some(CsrSeed {
            key_schema,
            value_schema,
        })
    }

    Ok(())
}

pub async fn purify_csv(
    file: Option<File>,
    connector: &CreateSourceConnector,
    delimiter: char,
    columns: &mut CsvColumns,
) -> anyhow::Result<()> {
    if matches!(columns, CsvColumns::Header { .. })
        && !matches!(
            connector,
            CreateSourceConnector::File { .. } | CreateSourceConnector::S3 { .. }
        )
    {
        bail_unsupported!("CSV WITH HEADER with non-file or S3 sources");
    }

    let first_row = if let Some(file) = file {
        let file = tokio::io::BufReader::new(file);
        let csv_header = file.lines().next_line().await;
        if !delimiter.is_ascii() {
            bail!("CSV delimiter must be ascii");
        }
        match csv_header {
            Ok(Some(csv_header)) => {
                let mut reader = ReaderBuilder::new()
                    .delimiter(delimiter as u8)
                    .has_headers(false)
                    .from_reader(csv_header.as_bytes());

                if let Some(result) = reader.records().next() {
                    match result {
                        Ok(headers) => Some(headers),
                        Err(e) => bail!("Unable to parse header row: {}", e),
                    }
                } else {
                    None
                }
            }
            Ok(None) => {
                if let CsvColumns::Header { names } = columns {
                    if names.is_empty() {
                        bail!(
                            "CSV file expected to have at least one line \
                             to determine column names, but is empty"
                        );
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            Err(e) => {
                // TODO(#7562): support compressed files
                if let CsvColumns::Header { names } = columns {
                    if names.is_empty() {
                        bail!("Cannot determine header by reading CSV file: {}", e);
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        }
    } else {
        None
    };

    match (&columns, first_row) {
        (CsvColumns::Header { names }, Some(headers)) if names.is_empty() => {
            *columns = CsvColumns::Header {
                names: headers.into_iter().map(Ident::from).collect(),
            };
        }
        (CsvColumns::Header { names }, Some(headers)) => {
            if names.len() != headers.len() {
                bail!(
                    "Named column count ({}) does not match \
                     number of columns discovered ({})",
                    names.len(),
                    headers.len()
                );
            } else if let Some((sql, csv)) = names
                .iter()
                .zip(headers.iter())
                .find(|(sql, csv)| sql.as_str() != &**csv)
            {
                bail!(
                    "Header columns do not match named columns from CREATE SOURCE statement. \
                               First mismatched columns: {} != {}",
                    sql,
                    csv
                );
            }
        }
        (CsvColumns::Header { names }, None) if names.is_empty() => match connector {
            CreateSourceConnector::File { .. } => {
                bail!("CSV WITH HEADER requires a way to determine the header row, but does not exist")
            }
            CreateSourceConnector::S3 { .. } => {
                bail!("CSV WITH HEADER for S3 sources requiers specifying the header columns")
            }
            _ => bail!("CSV WITH HEADER is only supported for S3 and file sources"),
        },
        (CsvColumns::Header { names }, None) => {
            // we don't need to do any verification if we are told the names of the headers
            assert!(
                !names.is_empty(),
                "empty names should be caught in a previous match arm"
            );
        }

        (CsvColumns::Count(n), first_line) => {
            if let Some(columns) = first_line {
                if *n != columns.len() {
                    bail!(
                        "Specified column count (WITH {} COLUMNS) \
                                 does not match number of columns in CSV file ({})",
                        n,
                        columns.len()
                    );
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct Schema {
    pub key_schema: Option<String>,
    pub value_schema: String,
    pub schema_registry_config: Option<mz_ccsr::ClientConfig>,
    pub confluent_wire_format: bool,
}

async fn get_remote_csr_schema(
    schema_registry_config: mz_ccsr::ClientConfig,
    topic: String,
) -> Result<Schema, anyhow::Error> {
    let ccsr_client = schema_registry_config.clone().build()?;

    let value_schema_name = format!("{}-value", topic);
    let value_schema = ccsr_client
        .get_schema_by_subject(&value_schema_name)
        .await
        .with_context(|| {
            format!(
                "fetching latest schema for subject '{}' from registry",
                value_schema_name
            )
        })?;
    let subject = format!("{}-key", topic);
    let key_schema = match ccsr_client.get_schema_by_subject(&subject).await {
        Ok(ks) => Some(ks),
        Err(GetBySubjectError::SubjectNotFound) => None,
        Err(e) => bail!(e),
    };
    Ok(Schema {
        key_schema: key_schema.map(|s| s.raw),
        value_schema: value_schema.raw,
        schema_registry_config: Some(schema_registry_config),
        confluent_wire_format: true,
    })
}

/// Collect protobuf message descriptor from CSR and compile the descriptor.
async fn compile_proto(
    subject_name: &String,
    ccsr_client: Client,
) -> Result<CsrSeedCompiledEncoding, anyhow::Error> {
    let (primary_subject, dependency_subjects) =
        ccsr_client.get_subject_and_references(subject_name).await?;

    // Compile .proto files into a file descriptor set.
    let mut source_tree = VirtualSourceTree::new();
    for subject in iter::once(&primary_subject).chain(dependency_subjects.iter()) {
        source_tree.as_mut().add_file(
            Path::new(&subject.name),
            subject.schema.raw.as_bytes().to_vec(),
        );
    }
    let mut db = SourceTreeDescriptorDatabase::new(source_tree.as_mut());
    let fds = db
        .as_mut()
        .build_file_descriptor_set(&[Path::new(&primary_subject.name)])?;

    // Ensure there is exactly one message in the file.
    let primary_fd = fds.file(0);
    let message_name = match primary_fd.message_type_size() {
        1 => String::from_utf8_lossy(primary_fd.message_type(0).name()).into_owned(),
        0 => bail_unsupported!(9598, "Protobuf schemas with no messages"),
        _ => bail_unsupported!(9598, "Protobuf schemas with multiple messages"),
    };

    // Encode the file descriptor set into a SQL byte string.
    let mut schema = String::new();
    strconv::format_bytes(&mut schema, &fds.serialize()?);

    Ok(CsrSeedCompiledEncoding {
        schema,
        message_name,
    })
}

/// Makes an always-valid AWS API call to perform a basic sanity check of
/// whether the specified AWS configuration is valid.
async fn validate_aws_credentials(
    config: &AwsConfig,
    external_id: AwsExternalId,
) -> Result<(), anyhow::Error> {
    let config = config.load(external_id).await;
    let sts_client = mz_aws_util::sts::client(&config);
    let _ = sts_client
        .get_caller_identity()
        .send()
        .await
        .context("Unable to validate AWS credentials")?;
    Ok(())
}
