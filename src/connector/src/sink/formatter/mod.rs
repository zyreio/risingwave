// Copyright 2024 RisingWave Labs
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

use anyhow::anyhow;
use risingwave_common::array::StreamChunk;

use crate::sink::{Result, SinkError};

mod append_only;
mod debezium_json;
mod upsert;

pub use append_only::AppendOnlyFormatter;
pub use debezium_json::{DebeziumAdapterOpts, DebeziumJsonFormatter};
use risingwave_common::catalog::Schema;
use risingwave_common::types::DataType;
pub use upsert::UpsertFormatter;

use super::catalog::{SinkEncode, SinkFormat, SinkFormatDesc};
use super::encoder::template::TemplateEncoder;
use super::encoder::text::TextEncoder;
use super::encoder::{
    DateHandlingMode, JsonbHandlingMode, KafkaConnectParams, TimeHandlingMode,
    TimestamptzHandlingMode,
};
use super::redis::{KEY_FORMAT, VALUE_FORMAT};
use crate::sink::encoder::{
    AvroEncoder, AvroHeader, JsonEncoder, ProtoEncoder, ProtoHeader, TimestampHandlingMode,
};

/// Transforms a `StreamChunk` into a sequence of key-value pairs according a specific format,
/// for example append-only, upsert or debezium.
pub trait SinkFormatter {
    type K;
    type V;

    /// * Key may be None so that messages are partitioned using round-robin.
    ///   For example append-only without `primary_key` (aka `downstream_pk`) set.
    /// * Value may be None so that messages with same key are removed during log compaction.
    ///   For example debezium tombstone event.
    fn format_chunk(
        &self,
        chunk: &StreamChunk,
    ) -> impl Iterator<Item = Result<(Option<Self::K>, Option<Self::V>)>>;
}

/// `tri!` in generators yield `Err` and return `()`
/// `?` in generators return `Err`
#[macro_export]
macro_rules! tri {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(err) => {
                yield Err(err);
                return;
            }
        }
    };
}

pub enum SinkFormatterImpl {
    // append-only
    AppendOnlyJson(AppendOnlyFormatter<JsonEncoder, JsonEncoder>),
    AppendOnlyTextJson(AppendOnlyFormatter<TextEncoder, JsonEncoder>),
    AppendOnlyAvro(AppendOnlyFormatter<AvroEncoder, AvroEncoder>),
    AppendOnlyTextAvro(AppendOnlyFormatter<TextEncoder, AvroEncoder>),
    AppendOnlyProto(AppendOnlyFormatter<JsonEncoder, ProtoEncoder>),
    AppendOnlyTextProto(AppendOnlyFormatter<TextEncoder, ProtoEncoder>),
    AppendOnlyTemplate(AppendOnlyFormatter<TemplateEncoder, TemplateEncoder>),
    AppendOnlyTextTemplate(AppendOnlyFormatter<TextEncoder, TemplateEncoder>),
    // upsert
    UpsertJson(UpsertFormatter<JsonEncoder, JsonEncoder>),
    UpsertTextJson(UpsertFormatter<TextEncoder, JsonEncoder>),
    UpsertAvro(UpsertFormatter<AvroEncoder, AvroEncoder>),
    UpsertTextAvro(UpsertFormatter<TextEncoder, AvroEncoder>),
    UpsertProto(UpsertFormatter<JsonEncoder, ProtoEncoder>),
    UpsertTextProto(UpsertFormatter<TextEncoder, ProtoEncoder>),
    UpsertTemplate(UpsertFormatter<TemplateEncoder, TemplateEncoder>),
    UpsertTextTemplate(UpsertFormatter<TextEncoder, TemplateEncoder>),
    // debezium
    DebeziumJson(DebeziumJsonFormatter),
}

#[derive(Debug, Clone)]
pub struct EncoderParams<'a> {
    format_desc: &'a SinkFormatDesc,
    schema: Schema,
    db_name: String,
    sink_from_name: String,
    topic: &'a str,
}

/// Each encoder shall be able to be built from parameters.
///
/// This is not part of `RowEncoder` trait, because that one is about how an encoder completes its
/// own job as a self-contained unit, with a custom `new` asking for only necessary info; while this
/// one is about how different encoders can be selected from a common SQL interface.
pub trait EncoderBuild: Sized {
    /// Pass `pk_indices: None` for value/payload and `Some` for key. Certain encoder builds
    /// differently when used as key vs value.
    async fn build(params: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self>;
}

impl EncoderBuild for JsonEncoder {
    async fn build(b: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self> {
        let timestamptz_mode = TimestamptzHandlingMode::from_options(&b.format_desc.options)?;
        let jsonb_handling_mode = JsonbHandlingMode::from_options(&b.format_desc.options)?;
        let encoder = JsonEncoder::new(
            b.schema,
            pk_indices,
            DateHandlingMode::FromCe,
            TimestampHandlingMode::Milli,
            timestamptz_mode,
            TimeHandlingMode::Milli,
            jsonb_handling_mode,
        );
        let encoder = if let Some(s) = b.format_desc.options.get("schemas.enable") {
            match s.to_lowercase().parse::<bool>() {
                Ok(true) => {
                    let kafka_connect = KafkaConnectParams {
                        schema_name: format!("{}.{}", b.db_name, b.sink_from_name),
                    };
                    encoder.with_kafka_connect(kafka_connect)
                }
                Ok(false) => encoder,
                _ => {
                    return Err(SinkError::Config(anyhow!(
                        "schemas.enable is expected to be `true` or `false`, got {s}",
                    )));
                }
            }
        } else {
            encoder
        };
        Ok(encoder)
    }
}

impl EncoderBuild for ProtoEncoder {
    async fn build(b: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self> {
        // TODO: better to be a compile-time assert
        assert!(pk_indices.is_none());
        // By passing `None` as `aws_auth_props`, reading from `s3://` not supported yet.
        let (descriptor, sid) =
            crate::schema::protobuf::fetch_descriptor(&b.format_desc.options, b.topic, None)
                .await
                .map_err(|e| SinkError::Config(anyhow!(e)))?;
        let header = match sid {
            None => ProtoHeader::None,
            Some(sid) => ProtoHeader::ConfluentSchemaRegistry(sid),
        };
        ProtoEncoder::new(b.schema, None, descriptor, header)
    }
}

impl EncoderBuild for TextEncoder {
    async fn build(params: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self> {
        let Some(pk_indices) = pk_indices else {
            return Err(SinkError::Config(anyhow!(
                "TextEncoder requires primary key columns to be specified"
            )));
        };
        if pk_indices.len() != 1 {
            return Err(SinkError::Config(anyhow!(
                    "The key encode is TEXT, but the primary key has {} columns. The key encode TEXT requires the primary key to be a single column",
                    pk_indices.len()
                )));
        }

        let schema_ref = params.schema.fields().get(pk_indices[0]).ok_or_else(|| {
            SinkError::Config(anyhow!(
                "The primary key column index {} is out of bounds in schema {:?}",
                pk_indices[0],
                params.schema
            ))
        })?;
        match &schema_ref.data_type() {
            DataType::Varchar
            | DataType::Boolean
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::Int256
            | DataType::Serial => {}
            _ => {
                // why we don't allow float as text for key encode: https://github.com/risingwavelabs/risingwave/pull/16377#discussion_r1591864960
                return Err(SinkError::Config(
                    anyhow!(
                            "The key encode is TEXT, but the primary key column {} has type {}. The key encode TEXT requires the primary key column to be of type varchar, bool, small int, int, big int, serial or rw_int256.",
                            schema_ref.name,
                            schema_ref.data_type
                        ),
                ));
            }
        }

        Ok(Self::new(params.schema, pk_indices[0]))
    }
}

impl EncoderBuild for AvroEncoder {
    async fn build(b: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self> {
        let loader =
            crate::schema::SchemaLoader::from_format_options(b.topic, &b.format_desc.options)
                .map_err(|e| SinkError::Config(anyhow!(e)))?;

        let (schema_id, avro) = match pk_indices {
            Some(_) => loader
                .load_key_schema()
                .await
                .map_err(|e| SinkError::Config(anyhow!(e)))?,
            None => loader
                .load_val_schema()
                .await
                .map_err(|e| SinkError::Config(anyhow!(e)))?,
        };
        AvroEncoder::new(
            b.schema,
            pk_indices,
            std::sync::Arc::new(avro),
            AvroHeader::ConfluentSchemaRegistry(schema_id),
        )
    }
}

impl EncoderBuild for TemplateEncoder {
    async fn build(b: EncoderParams<'_>, pk_indices: Option<Vec<usize>>) -> Result<Self> {
        let option_name = match pk_indices {
            Some(_) => KEY_FORMAT,
            None => VALUE_FORMAT,
        };
        let template = b.format_desc.options.get(option_name).ok_or_else(|| {
            SinkError::Config(anyhow!(
                "Cannot find '{option_name}',please set it or use JSON"
            ))
        })?;
        Ok(TemplateEncoder::new(b.schema, pk_indices, template.clone()))
    }
}

struct FormatterParams<'a> {
    builder: EncoderParams<'a>,
    pk_indices: Vec<usize>,
}

/// Each formatter shall be able to be built from parameters.
///
/// This is not part of `SinkFormatter` trait, because that is about how a formatter completes its
/// own job as a self-contained unit, with a custom `new` asking for only necessary info; while this
/// one is about how different formatters can be selected from a common SQL interface.
trait FormatterBuild: Sized {
    async fn build(b: FormatterParams<'_>) -> Result<Self>;
}

impl<KE: EncoderBuild, VE: EncoderBuild> FormatterBuild for AppendOnlyFormatter<KE, VE> {
    async fn build(b: FormatterParams<'_>) -> Result<Self> {
        let key_encoder = match b.pk_indices.is_empty() {
            true => None,
            false => Some(KE::build(b.builder.clone(), Some(b.pk_indices)).await?),
        };
        let val_encoder = VE::build(b.builder, None).await?;
        Ok(AppendOnlyFormatter::new(key_encoder, val_encoder))
    }
}

impl<KE: EncoderBuild, VE: EncoderBuild> FormatterBuild for UpsertFormatter<KE, VE> {
    async fn build(b: FormatterParams<'_>) -> Result<Self> {
        let key_encoder = KE::build(b.builder.clone(), Some(b.pk_indices)).await?;
        let val_encoder = VE::build(b.builder, None).await?;
        Ok(UpsertFormatter::new(key_encoder, val_encoder))
    }
}

impl FormatterBuild for DebeziumJsonFormatter {
    async fn build(b: FormatterParams<'_>) -> Result<Self> {
        assert_eq!(b.builder.format_desc.encode, SinkEncode::Json);

        Ok(DebeziumJsonFormatter::new(
            b.builder.schema,
            b.pk_indices,
            b.builder.db_name,
            b.builder.sink_from_name,
            DebeziumAdapterOpts::default(),
        ))
    }
}

impl SinkFormatterImpl {
    pub async fn new(
        format_desc: &SinkFormatDesc,
        schema: Schema,
        pk_indices: Vec<usize>,
        db_name: String,
        sink_from_name: String,
        topic: &str,
    ) -> Result<Self> {
        use {SinkEncode as E, SinkFormat as F, SinkFormatterImpl as Impl};
        let p = FormatterParams {
            builder: EncoderParams {
                format_desc,
                schema,
                db_name,
                sink_from_name,
                topic,
            },
            pk_indices,
        };

        // When defining `SinkFormatterImpl` we already linked each variant (eg `AppendOnlyJson`) to
        // an instantiation (eg `AppendOnlyFormatter<JsonEncoder, JsonEncoder>`) that implements the
        // trait `FormatterBuild`.
        //
        // Here we just need to match a `(format, encode)` to a variant, and rustc shall be able to
        // find the corresponding instantiation.

        // However,
        //   `Impl::AppendOnlyJson(FormatterBuild::build(p).await?)`
        // fails to be inferred without the following dummy wrapper.
        async fn build<T: FormatterBuild>(p: FormatterParams<'_>) -> Result<T> {
            T::build(p).await
        }

        Ok(
            match (
                &format_desc.format,
                &format_desc.encode,
                &format_desc.key_encode,
            ) {
                (F::AppendOnly, E::Json, Some(E::Text)) => {
                    Impl::AppendOnlyTextJson(build(p).await?)
                }
                (F::AppendOnly, E::Json, None) => Impl::AppendOnlyJson(build(p).await?),
                (F::AppendOnly, E::Avro, Some(E::Text)) => {
                    Impl::AppendOnlyTextAvro(build(p).await?)
                }
                (F::AppendOnly, E::Avro, None) => Impl::AppendOnlyAvro(build(p).await?),
                (F::AppendOnly, E::Protobuf, Some(E::Text)) => {
                    Impl::AppendOnlyTextProto(build(p).await?)
                }
                (F::AppendOnly, E::Protobuf, None) => Impl::AppendOnlyProto(build(p).await?),
                (F::AppendOnly, E::Template, Some(E::Text)) => {
                    Impl::AppendOnlyTextTemplate(build(p).await?)
                }
                (F::AppendOnly, E::Template, None) => Impl::AppendOnlyTemplate(build(p).await?),
                (F::Upsert, E::Json, Some(E::Text)) => Impl::UpsertTextJson(build(p).await?),
                (F::Upsert, E::Json, None) => Impl::UpsertJson(build(p).await?),
                (F::Upsert, E::Avro, Some(E::Text)) => Impl::UpsertTextAvro(build(p).await?),
                (F::Upsert, E::Avro, None) => Impl::UpsertAvro(build(p).await?),
                (F::Upsert, E::Protobuf, Some(E::Text)) => Impl::UpsertTextProto(build(p).await?),
                (F::Upsert, E::Protobuf, None) => Impl::UpsertProto(build(p).await?),
                (F::Upsert, E::Template, Some(E::Text)) => {
                    Impl::UpsertTextTemplate(build(p).await?)
                }
                (F::Upsert, E::Template, None) => Impl::UpsertTemplate(build(p).await?),
                (F::Debezium, E::Json, None) => Impl::DebeziumJson(build(p).await?),
                (F::AppendOnly | F::Upsert, E::Text, _) => {
                    return Err(SinkError::Config(anyhow!(
                        "ENCODE TEXT is only valid as key encode."
                    )));
                }
                (F::AppendOnly, E::Avro, _)
                | (F::Upsert, E::Protobuf, _)
                | (F::Debezium, E::Json, Some(_))
                | (F::Debezium, E::Avro | E::Protobuf | E::Template | E::Text, _)
                | (F::AppendOnly | F::Upsert, _, Some(E::Template) | Some(E::Json) | Some(E::Avro) | Some(E::Protobuf)) // reject other encode as key encode
                => {
                    return Err(SinkError::Config(anyhow!(
                        "sink format/encode/key_encode unsupported: {:?} {:?} {:?}",
                        format_desc.format,
                        format_desc.encode,
                        format_desc.key_encode
                    )));
                }
            },
        )
    }
}

#[macro_export]
macro_rules! dispatch_sink_formatter_impl {
    ($impl:expr, $name:ident, $body:expr) => {
        match $impl {
            SinkFormatterImpl::AppendOnlyJson($name) => $body,
            SinkFormatterImpl::AppendOnlyTextJson($name) => $body,
            SinkFormatterImpl::AppendOnlyAvro($name) => $body,
            SinkFormatterImpl::AppendOnlyTextAvro($name) => $body,
            SinkFormatterImpl::AppendOnlyProto($name) => $body,
            SinkFormatterImpl::AppendOnlyTextProto($name) => $body,
            SinkFormatterImpl::UpsertTextProto($name) => $body,
            SinkFormatterImpl::UpsertProto($name) => $body,

            SinkFormatterImpl::UpsertJson($name) => $body,
            SinkFormatterImpl::UpsertTextJson($name) => $body,
            SinkFormatterImpl::UpsertAvro($name) => $body,
            SinkFormatterImpl::UpsertTextAvro($name) => $body,
            SinkFormatterImpl::DebeziumJson($name) => $body,
            SinkFormatterImpl::AppendOnlyTextTemplate($name) => $body,
            SinkFormatterImpl::AppendOnlyTemplate($name) => $body,
            SinkFormatterImpl::UpsertTextTemplate($name) => $body,
            SinkFormatterImpl::UpsertTemplate($name) => $body,
        }
    };
}

#[macro_export]
macro_rules! dispatch_sink_formatter_str_key_impl {
    ($impl:expr, $name:ident, $body:expr) => {
        match $impl {
            SinkFormatterImpl::AppendOnlyJson($name) => $body,
            SinkFormatterImpl::AppendOnlyTextJson($name) => $body,
            SinkFormatterImpl::AppendOnlyAvro(_) => unreachable!(),
            SinkFormatterImpl::AppendOnlyTextAvro($name) => $body,
            SinkFormatterImpl::AppendOnlyProto($name) => $body,
            SinkFormatterImpl::AppendOnlyTextProto($name) => $body,
            SinkFormatterImpl::UpsertTextProto($name) => $body,
            SinkFormatterImpl::UpsertProto($name) => $body,

            SinkFormatterImpl::UpsertJson($name) => $body,
            SinkFormatterImpl::UpsertTextJson($name) => $body,
            SinkFormatterImpl::UpsertAvro(_) => unreachable!(),
            SinkFormatterImpl::UpsertTextAvro($name) => $body,
            SinkFormatterImpl::DebeziumJson($name) => $body,
            SinkFormatterImpl::AppendOnlyTextTemplate($name) => $body,
            SinkFormatterImpl::AppendOnlyTemplate($name) => $body,
            SinkFormatterImpl::UpsertTextTemplate($name) => $body,
            SinkFormatterImpl::UpsertTemplate($name) => $body,
        }
    };
}
