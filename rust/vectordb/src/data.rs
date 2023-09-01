// Copyright 2023 Lance Developers.
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

//! Data types, schema coercion, and data cleaning and etc.

use std::sync::Arc;

use arrow_array::{
    cast::AsArray,
    types::{Float16Type, Float32Type, Float64Type},
    Array, ArrowNumericType, FixedSizeListArray, PrimitiveArray, RecordBatch, RecordBatchIterator,
    RecordBatchReader,
};
use arrow_cast::{can_cast_types, cast};
use arrow_schema::{ArrowError, DataType, Field, Schema};
use half::f16;
use lance::arrow::{DataTypeExt, FixedSizeListArrayExt};
use log::warn;
use num_traits::cast::AsPrimitive;

use crate::error::Result;

fn cast_array<I: ArrowNumericType, O: ArrowNumericType>(
    arr: &PrimitiveArray<I>,
) -> Arc<PrimitiveArray<O>>
where
    I::Native: AsPrimitive<O::Native>,
{
    Arc::new(PrimitiveArray::<O>::from_iter_values(
        arr.values().iter().map(|v| (*v).as_()),
    ))
}

fn cast_float_array<I: ArrowNumericType>(
    arr: &PrimitiveArray<I>,
    dt: &DataType,
) -> std::result::Result<Arc<dyn Array>, ArrowError>
where
    I::Native: AsPrimitive<f64> + AsPrimitive<f32> + AsPrimitive<f16>,
{
    match dt {
        DataType::Float16 => Ok(cast_array::<I, Float16Type>(arr)),
        DataType::Float32 => Ok(cast_array::<I, Float32Type>(arr)),
        DataType::Float64 => Ok(cast_array::<I, Float64Type>(arr)),
        _ => Err(ArrowError::SchemaError(format!(
            "Incompatible change field: unable to coerce {:?} to {:?}",
            arr.data_type(),
            dt
        ))),
    }
}

fn coerce_array(
    array: &Arc<dyn Array>,
    field: &Field,
) -> std::result::Result<Arc<dyn Array>, ArrowError> {
    if array.data_type() == field.data_type() {
        return Ok(array.clone());
    }
    match (array.data_type(), field.data_type()) {
        // Normal cast-able types.
        (adt, dt) if can_cast_types(adt, dt) => cast(&array, dt),
        // Casting between f16/f32/f64 can be lossy.
        (adt, dt) if (adt.is_floating() || dt.is_floating()) => {
            if adt.byte_width() < dt.byte_width() {
                warn!(
                    "Coercing field {} {:?} to {:?} might lose precision",
                    field.name(),
                    adt,
                    dt
                );
            }
            match adt {
                DataType::Float16 => cast_float_array(array.as_primitive::<Float16Type>(), dt),
                DataType::Float32 => cast_float_array(array.as_primitive::<Float32Type>(), dt),
                DataType::Float64 => cast_float_array(array.as_primitive::<Float64Type>(), dt),
                _ => unreachable!(),
            }
        }
        (adt, DataType::FixedSizeList(exp_field, exp_dim)) => match adt {
            // Cast a float fixed size array with same dimension to the expected type.
            DataType::FixedSizeList(_, dim) if dim == exp_dim => {
                let actual_sub = array.as_fixed_size_list();
                let values = coerce_array(&actual_sub.values(), exp_field)?;
                Ok(Arc::new(FixedSizeListArray::try_new_from_values(
                    values.clone(),
                    *dim,
                )?) as Arc<dyn Array>)
            }
            DataType::List(sub_field) => {
                todo!("cast list to fixed size list")
            }
            _ => Err(ArrowError::SchemaError(format!(
                "Incompatible coerce fixed size list: unable to coerce {:?} from {:?}",
                field,
                array.data_type()
            )))?,
        },
        _ => Err(ArrowError::SchemaError(format!(
            "Incompatible change field {}: unable to coerce {:?} to {:?}",
            field.name(),
            array.data_type(),
            field.data_type()
        )))?,
    }
}

fn coerce_schema_batch(
    batch: RecordBatch,
    schema: Arc<Schema>,
) -> std::result::Result<RecordBatch, ArrowError> {
    if batch.schema() == schema {
        return Ok(batch);
    }
    let columns = schema
        .fields()
        .iter()
        .map(|field| {
            batch
                .column_by_name(field.name())
                .map(|c| coerce_array(c, field))
                .ok_or(|| {
                    ArrowError::SchemaError(format!("Column {} not found in batch", field.name()))
                })
        })
        .flatten()
        .collect::<std::result::Result<Vec<_>, ArrowError>>()?;
    RecordBatch::try_new(schema, columns)
}

/// Coerce the batch reader schema, to match the given [Schema].
///
pub fn coerce_schema(
    reader: impl RecordBatchReader,
    schema: Arc<Schema>,
) -> Result<impl RecordBatchReader> {
    if reader.schema() == schema {
        return Ok(RecordBatchIterator::new(
            reader.into_iter().collect::<Vec<_>>(),
            schema,
        ));
    }
    let batches = reader
        .map(|batch| coerce_schema_batch(batch?, schema.clone()))
        .collect::<Vec<_>>();
    Ok(RecordBatchIterator::new(batches, schema))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use arrow_array::{
        FixedSizeListArray, Float16Array, Float32Array, Float64Array, Int32Array, Int8Array,
        RecordBatch, RecordBatchIterator, StringArray,
    };
    use arrow_schema::Field;
    use half::f16;
    use lance::arrow::FixedSizeListArrayExt;

    #[test]
    fn test_coerce_list_to_fixed_size_list() {
        let schema = Arc::new(Schema::new(vec![
            Field::new(
                "fl",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), 64),
                true,
            ),
            Field::new("s", DataType::Utf8, true),
            Field::new("f", DataType::Float16, true),
            Field::new("i", DataType::Int32, true),
        ]));

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(
                    FixedSizeListArray::try_new_from_values(
                        Float32Array::from_iter_values((0..256).map(|v| v as f32)),
                        64,
                    )
                    .unwrap(),
                ),
                Arc::new(StringArray::from(vec![
                    Some("hello"),
                    Some("world"),
                    Some("from"),
                    Some("lance"),
                ])),
                Arc::new(Float16Array::from_iter_values(
                    (0..4).map(|v| f16::from_f32(v as f32)),
                )),
                Arc::new(Int32Array::from_iter_values(0..4)),
            ],
        )
        .unwrap();
        let reader =
            RecordBatchIterator::new(vec![batch.clone()].into_iter().map(Ok), schema.clone());

        let expected_schema = Arc::new(Schema::new(vec![
            Field::new(
                "fl",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float16, true)), 64),
                true,
            ),
            Field::new("s", DataType::Utf8, true),
            Field::new("f", DataType::Float64, true),
            Field::new("i", DataType::Int8, true),
        ]));
        let stream = coerce_schema(reader, expected_schema.clone()).unwrap();
        let batches = stream.collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        let batch = batches[0].as_ref().unwrap();
        assert_eq!(batch.schema(), expected_schema);

        let expected = RecordBatch::try_new(
            expected_schema,
            vec![
                Arc::new(
                    FixedSizeListArray::try_new_from_values(
                        Float16Array::from_iter_values((0..256).map(|v| f16::from_f32(v as f32))),
                        64,
                    )
                    .unwrap(),
                ),
                Arc::new(StringArray::from(vec![
                    Some("hello"),
                    Some("world"),
                    Some("from"),
                    Some("lance"),
                ])),
                Arc::new(Float64Array::from_iter_values((0..4).map(|v| v as f64))),
                Arc::new(Int8Array::from_iter_values(0..4)),
            ],
        )
        .unwrap();
        assert_eq!(batch, &expected);
    }
}
