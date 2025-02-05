// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! InList expression

use std::any::Any;
use std::collections::HashSet;
use std::sync::Arc;

use arrow::array::GenericStringArray;
use arrow::array::{
    ArrayRef, BooleanArray, Float32Array, Float64Array, Int16Array, Int32Array,
    Int64Array, Int8Array, OffsetSizeTrait, UInt16Array, UInt32Array, UInt64Array,
    UInt8Array,
};
use arrow::datatypes::ArrowPrimitiveType;
use arrow::{
    datatypes::{DataType, Schema},
    record_batch::RecordBatch,
};

use crate::{expressions, PhysicalExpr};
use arrow::array::*;
use arrow::buffer::{Buffer, MutableBuffer};
use datafusion_common::ScalarValue;
use datafusion_common::{DataFusionError, Result};
use datafusion_expr::ColumnarValue;

/// Size at which to use a Set rather than Vec for `IN` / `NOT IN`
/// Value chosen by the benchmark at
/// https://github.com/apache/arrow-datafusion/pull/2156#discussion_r845198369
/// TODO: add switch codeGen in In_List
static OPTIMIZER_INSET_THRESHOLD: usize = 30;

macro_rules! compare_op_scalar {
    ($left: expr, $right:expr, $op:expr) => {{
        let null_bit_buffer = $left.data().null_buffer().cloned();

        let comparison =
            (0..$left.len()).map(|i| unsafe { $op($left.value_unchecked(i), $right) });
        // same as $left.len()
        let buffer = unsafe { MutableBuffer::from_trusted_len_iter_bool(comparison) };

        let data = unsafe {
            ArrayData::new_unchecked(
                DataType::Boolean,
                $left.len(),
                None,
                null_bit_buffer,
                0,
                vec![Buffer::from(buffer)],
                vec![],
            )
        };
        Ok(BooleanArray::from(data))
    }};
}

/// InList
#[derive(Debug)]
pub struct InListExpr {
    expr: Arc<dyn PhysicalExpr>,
    list: Vec<Arc<dyn PhysicalExpr>>,
    negated: bool,
    set: Option<InSet>,
}

/// InSet
#[derive(Debug)]
pub struct InSet {
    set: HashSet<ScalarValue>,
}

impl InSet {
    pub fn new(set: HashSet<ScalarValue>) -> Self {
        Self { set }
    }

    pub fn get_set(&self) -> &HashSet<ScalarValue> {
        &self.set
    }
}

macro_rules! make_contains {
    ($ARRAY:expr, $LIST_VALUES:expr, $NEGATED:expr, $SCALAR_VALUE:ident, $ARRAY_TYPE:ident) => {{
        let array = $ARRAY.as_any().downcast_ref::<$ARRAY_TYPE>().unwrap();

        let contains_null = $LIST_VALUES
            .iter()
            .any(|v| matches!(v, ColumnarValue::Scalar(s) if s.is_null()));
        let values = $LIST_VALUES
            .iter()
            .flat_map(|expr| match expr {
                ColumnarValue::Scalar(s) => match s {
                    ScalarValue::$SCALAR_VALUE(Some(v)) => Some(*v),
                    ScalarValue::$SCALAR_VALUE(None) => None,
                    ScalarValue::Utf8(None) => None,
                    datatype => unimplemented!("Unexpected type {} for InList", datatype),
                },
                ColumnarValue::Array(_) => {
                    unimplemented!("InList does not yet support nested columns.")
                }
            })
            .collect::<Vec<_>>();

        Ok(ColumnarValue::Array(Arc::new(
            array
                .iter()
                .map(|x| {
                    let contains = x.map(|x| values.contains(&x));
                    match contains {
                        Some(true) => {
                            if $NEGATED {
                                Some(false)
                            } else {
                                Some(true)
                            }
                        }
                        Some(false) => {
                            if contains_null {
                                None
                            } else if $NEGATED {
                                Some(true)
                            } else {
                                Some(false)
                            }
                        }
                        None => None,
                    }
                })
                .collect::<BooleanArray>(),
        )))
    }};
}

macro_rules! make_contains_primitive {
    ($ARRAY:expr, $LIST_VALUES:expr, $NEGATED:expr, $SCALAR_VALUE:ident, $ARRAY_TYPE:ident) => {{
        let array = $ARRAY.as_any().downcast_ref::<$ARRAY_TYPE>().unwrap();

        let contains_null = $LIST_VALUES
            .iter()
            .any(|v| matches!(v, ColumnarValue::Scalar(s) if s.is_null()));
        let values = $LIST_VALUES
            .iter()
            .flat_map(|expr| match expr {
                ColumnarValue::Scalar(s) => match s {
                    ScalarValue::$SCALAR_VALUE(Some(v)) => Some(*v),
                    ScalarValue::$SCALAR_VALUE(None) => None,
                    ScalarValue::Utf8(None) => None,
                    datatype => unimplemented!("Unexpected type {} for InList", datatype),
                },
                ColumnarValue::Array(_) => {
                    unimplemented!("InList does not yet support nested columns.")
                }
            })
            .collect::<Vec<_>>();

        if $NEGATED {
            if contains_null {
                Ok(ColumnarValue::Array(Arc::new(
                    array
                        .iter()
                        .map(|x| match x.map(|v| !values.contains(&v)) {
                            Some(true) => None,
                            x => x,
                        })
                        .collect::<BooleanArray>(),
                )))
            } else {
                Ok(ColumnarValue::Array(Arc::new(
                    not_in_list_primitive(array, &values)?,
                )))
            }
        } else {
            if contains_null {
                Ok(ColumnarValue::Array(Arc::new(
                    array
                        .iter()
                        .map(|x| match x.map(|v| values.contains(&v)) {
                            Some(false) => None,
                            x => x,
                        })
                        .collect::<BooleanArray>(),
                )))
            } else {
                Ok(ColumnarValue::Array(Arc::new(in_list_primitive(
                    array, &values,
                )?)))
            }
        }
    }};
}

macro_rules! set_contains_with_negated {
    ($ARRAY:expr, $LIST_VALUES:expr, $NEGATED:expr) => {{
        if $NEGATED {
            return Ok(ColumnarValue::Array(Arc::new(
                $ARRAY
                    .iter()
                    .map(|x| x.map(|v| !$LIST_VALUES.contains(&v.try_into().unwrap())))
                    .collect::<BooleanArray>(),
            )));
        } else {
            return Ok(ColumnarValue::Array(Arc::new(
                $ARRAY
                    .iter()
                    .map(|x| x.map(|v| $LIST_VALUES.contains(&v.try_into().unwrap())))
                    .collect::<BooleanArray>(),
            )));
        }
    }};
}

// whether each value on the left (can be null) is contained in the non-null list
fn in_list_primitive<T: ArrowPrimitiveType>(
    array: &PrimitiveArray<T>,
    values: &[<T as ArrowPrimitiveType>::Native],
) -> Result<BooleanArray> {
    compare_op_scalar!(
        array,
        values,
        |x, v: &[<T as ArrowPrimitiveType>::Native]| v.contains(&x)
    )
}

// whether each value on the left (can be null) is contained in the non-null list
fn not_in_list_primitive<T: ArrowPrimitiveType>(
    array: &PrimitiveArray<T>,
    values: &[<T as ArrowPrimitiveType>::Native],
) -> Result<BooleanArray> {
    compare_op_scalar!(
        array,
        values,
        |x, v: &[<T as ArrowPrimitiveType>::Native]| !v.contains(&x)
    )
}

// whether each value on the left (can be null) is contained in the non-null list
fn in_list_utf8<OffsetSize: OffsetSizeTrait>(
    array: &GenericStringArray<OffsetSize>,
    values: &[&str],
) -> Result<BooleanArray> {
    compare_op_scalar!(array, values, |x, v: &[&str]| v.contains(&x))
}

fn not_in_list_utf8<OffsetSize: OffsetSizeTrait>(
    array: &GenericStringArray<OffsetSize>,
    values: &[&str],
) -> Result<BooleanArray> {
    compare_op_scalar!(array, values, |x, v: &[&str]| !v.contains(&x))
}

//check all filter values of In clause are static.
//include `CastExpr + Literal` or `Literal`
fn check_all_static_filter_expr(list: &[Arc<dyn PhysicalExpr>]) -> bool {
    list.iter().all(|v| {
        let cast = v.as_any().downcast_ref::<expressions::CastExpr>();
        if let Some(c) = cast {
            c.expr()
                .as_any()
                .downcast_ref::<expressions::Literal>()
                .is_some()
        } else {
            let cast = v.as_any().downcast_ref::<expressions::Literal>();
            cast.is_some()
        }
    })
}

fn cast_static_filter_to_set(list: &[Arc<dyn PhysicalExpr>]) -> HashSet<ScalarValue> {
    HashSet::from_iter(list.iter().map(|expr| {
        if let Some(cast) = expr.as_any().downcast_ref::<expressions::CastExpr>() {
            cast.expr()
                .as_any()
                .downcast_ref::<expressions::Literal>()
                .unwrap()
                .value()
                .clone()
        } else {
            expr.as_any()
                .downcast_ref::<expressions::Literal>()
                .unwrap()
                .value()
                .clone()
        }
    }))
}

impl InListExpr {
    /// Create a new InList expression
    pub fn new(
        expr: Arc<dyn PhysicalExpr>,
        list: Vec<Arc<dyn PhysicalExpr>>,
        negated: bool,
    ) -> Self {
        if list.len() > OPTIMIZER_INSET_THRESHOLD && check_all_static_filter_expr(&list) {
            Self {
                expr,
                set: Some(InSet::new(cast_static_filter_to_set(&list))),
                list,
                negated,
            }
        } else {
            Self {
                expr,
                list,
                negated,
                set: None,
            }
        }
    }

    /// Input expression
    pub fn expr(&self) -> &Arc<dyn PhysicalExpr> {
        &self.expr
    }

    /// List to search in
    pub fn list(&self) -> &[Arc<dyn PhysicalExpr>] {
        &self.list
    }

    /// Is this negated e.g. NOT IN LIST
    pub fn negated(&self) -> bool {
        self.negated
    }

    /// Compare for specific utf8 types
    #[allow(clippy::unnecessary_wraps)]
    fn compare_utf8<T: OffsetSizeTrait>(
        &self,
        array: ArrayRef,
        list_values: Vec<ColumnarValue>,
        negated: bool,
    ) -> Result<ColumnarValue> {
        let array = array
            .as_any()
            .downcast_ref::<GenericStringArray<T>>()
            .unwrap();

        let contains_null = list_values
            .iter()
            .any(|v| matches!(v, ColumnarValue::Scalar(s) if s.is_null()));
        let values = list_values
            .iter()
            .flat_map(|expr| match expr {
                ColumnarValue::Scalar(s) => match s {
                    ScalarValue::Utf8(Some(v)) => Some(v.as_str()),
                    ScalarValue::Utf8(None) => None,
                    ScalarValue::LargeUtf8(Some(v)) => Some(v.as_str()),
                    ScalarValue::LargeUtf8(None) => None,
                    datatype => unimplemented!("Unexpected type {} for InList", datatype),
                },
                ColumnarValue::Array(_) => {
                    unimplemented!("InList does not yet support nested columns.")
                }
            })
            .collect::<Vec<&str>>();

        if negated {
            if contains_null {
                Ok(ColumnarValue::Array(Arc::new(
                    array
                        .iter()
                        .map(|x| match x.map(|v| !values.contains(&v)) {
                            Some(true) => None,
                            x => x,
                        })
                        .collect::<BooleanArray>(),
                )))
            } else {
                Ok(ColumnarValue::Array(Arc::new(not_in_list_utf8(
                    array, &values,
                )?)))
            }
        } else if contains_null {
            Ok(ColumnarValue::Array(Arc::new(
                array
                    .iter()
                    .map(|x| match x.map(|v| values.contains(&v)) {
                        Some(false) => None,
                        x => x,
                    })
                    .collect::<BooleanArray>(),
            )))
        } else {
            Ok(ColumnarValue::Array(Arc::new(in_list_utf8(
                array, &values,
            )?)))
        }
    }
}

impl std::fmt::Display for InListExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if self.negated {
            if self.set.is_some() {
                write!(f, "{} NOT IN (SET) ({:?})", self.expr, self.list)
            } else {
                write!(f, "{} NOT IN ({:?})", self.expr, self.list)
            }
        } else if self.set.is_some() {
            write!(f, "Use {} IN (SET) ({:?})", self.expr, self.list)
        } else {
            write!(f, "{} IN ({:?})", self.expr, self.list)
        }
    }
}

impl PhysicalExpr for InListExpr {
    /// Return a reference to Any that can be used for downcasting
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self, _input_schema: &Schema) -> Result<DataType> {
        Ok(DataType::Boolean)
    }

    fn nullable(&self, input_schema: &Schema) -> Result<bool> {
        self.expr.nullable(input_schema)
    }

    fn evaluate(&self, batch: &RecordBatch) -> Result<ColumnarValue> {
        let value = self.expr.evaluate(batch)?;
        let value_data_type = value.data_type();

        if let Some(in_set) = &self.set {
            let array = match value {
                ColumnarValue::Array(array) => array,
                ColumnarValue::Scalar(scalar) => scalar.to_array(),
            };
            let set = in_set.get_set();
            match value_data_type {
                DataType::Boolean => {
                    let array = array.as_any().downcast_ref::<BooleanArray>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Int8 => {
                    let array = array.as_any().downcast_ref::<Int8Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Int16 => {
                    let array = array.as_any().downcast_ref::<Int16Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Int32 => {
                    let array = array.as_any().downcast_ref::<Int32Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Int64 => {
                    let array = array.as_any().downcast_ref::<Int64Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::UInt8 => {
                    let array = array.as_any().downcast_ref::<UInt8Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::UInt16 => {
                    let array = array.as_any().downcast_ref::<UInt16Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::UInt32 => {
                    let array = array.as_any().downcast_ref::<UInt32Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::UInt64 => {
                    let array = array.as_any().downcast_ref::<UInt64Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Float32 => {
                    let array = array.as_any().downcast_ref::<Float32Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Float64 => {
                    let array = array.as_any().downcast_ref::<Float64Array>().unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::Utf8 => {
                    let array = array
                        .as_any()
                        .downcast_ref::<GenericStringArray<i32>>()
                        .unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                DataType::LargeUtf8 => {
                    let array = array
                        .as_any()
                        .downcast_ref::<GenericStringArray<i64>>()
                        .unwrap();
                    set_contains_with_negated!(array, set, self.negated)
                }
                datatype => Result::Err(DataFusionError::NotImplemented(format!(
                    "InSet does not support datatype {:?}.",
                    datatype
                ))),
            }
        } else {
            let list_values = self
                .list
                .iter()
                .map(|expr| expr.evaluate(batch))
                .collect::<Result<Vec<_>>>()?;

            let array = match value {
                ColumnarValue::Array(array) => array,
                ColumnarValue::Scalar(scalar) => scalar.to_array(),
            };

            match value_data_type {
                DataType::Float32 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Float32,
                        Float32Array
                    )
                }
                DataType::Float64 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Float64,
                        Float64Array
                    )
                }
                DataType::Int16 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Int16,
                        Int16Array
                    )
                }
                DataType::Int32 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Int32,
                        Int32Array
                    )
                }
                DataType::Int64 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Int64,
                        Int64Array
                    )
                }
                DataType::Int8 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        Int8,
                        Int8Array
                    )
                }
                DataType::UInt16 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        UInt16,
                        UInt16Array
                    )
                }
                DataType::UInt32 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        UInt32,
                        UInt32Array
                    )
                }
                DataType::UInt64 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        UInt64,
                        UInt64Array
                    )
                }
                DataType::UInt8 => {
                    make_contains_primitive!(
                        array,
                        list_values,
                        self.negated,
                        UInt8,
                        UInt8Array
                    )
                }
                DataType::Boolean => {
                    make_contains!(
                        array,
                        list_values,
                        self.negated,
                        Boolean,
                        BooleanArray
                    )
                }
                DataType::Utf8 => {
                    self.compare_utf8::<i32>(array, list_values, self.negated)
                }
                DataType::LargeUtf8 => {
                    self.compare_utf8::<i64>(array, list_values, self.negated)
                }
                DataType::Null => {
                    let null_array = new_null_array(&DataType::Boolean, array.len());
                    Ok(ColumnarValue::Array(Arc::new(null_array)))
                }
                datatype => Result::Err(DataFusionError::NotImplemented(format!(
                    "InList does not support datatype {:?}.",
                    datatype
                ))),
            }
        }
    }
}

/// Creates a unary expression InList
pub fn in_list(
    expr: Arc<dyn PhysicalExpr>,
    list: Vec<Arc<dyn PhysicalExpr>>,
    negated: &bool,
) -> Result<Arc<dyn PhysicalExpr>> {
    Ok(Arc::new(InListExpr::new(expr, list, *negated)))
}

#[cfg(test)]
mod tests {
    use arrow::{array::StringArray, datatypes::Field};

    use super::*;
    use crate::expressions::{col, lit};
    use datafusion_common::Result;

    // applies the in_list expr to an input batch and list
    macro_rules! in_list {
        ($BATCH:expr, $LIST:expr, $NEGATED:expr, $EXPECTED:expr, $COL:expr) => {{
            let expr = in_list($COL, $LIST, $NEGATED).unwrap();
            let result = expr.evaluate(&$BATCH)?.into_array($BATCH.num_rows());
            let result = result
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("failed to downcast to BooleanArray");
            let expected = &BooleanArray::from($EXPECTED);
            assert_eq!(expected, result);
        }};
    }

    #[test]
    fn in_list_utf8() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Utf8, true)]);
        let a = StringArray::from(vec![Some("a"), Some("d"), None]);
        let col_a = col("a", &schema)?;
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a)])?;

        // expression: "a in ("a", "b")"
        let list = vec![
            lit(ScalarValue::Utf8(Some("a".to_string()))),
            lit(ScalarValue::Utf8(Some("b".to_string()))),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), Some(false), None],
            col_a.clone()
        );

        // expression: "a not in ("a", "b")"
        let list = vec![
            lit(ScalarValue::Utf8(Some("a".to_string()))),
            lit(ScalarValue::Utf8(Some("b".to_string()))),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), Some(true), None],
            col_a.clone()
        );

        // expression: "a not in ("a", "b")"
        let list = vec![
            lit(ScalarValue::Utf8(Some("a".to_string()))),
            lit(ScalarValue::Utf8(Some("b".to_string()))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), None, None],
            col_a.clone()
        );

        // expression: "a not in ("a", "b")"
        let list = vec![
            lit(ScalarValue::Utf8(Some("a".to_string()))),
            lit(ScalarValue::Utf8(Some("b".to_string()))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), None, None],
            col_a.clone()
        );

        Ok(())
    }

    #[test]
    fn in_list_int64() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, true)]);
        let a = Int64Array::from(vec![Some(0), Some(2), None]);
        let col_a = col("a", &schema)?;
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a)])?;

        // expression: "a in (0, 1)"
        let list = vec![
            lit(ScalarValue::Int64(Some(0))),
            lit(ScalarValue::Int64(Some(1))),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), Some(false), None],
            col_a.clone()
        );

        // expression: "a not in (0, 1)"
        let list = vec![
            lit(ScalarValue::Int64(Some(0))),
            lit(ScalarValue::Int64(Some(1))),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), Some(true), None],
            col_a.clone()
        );

        // expression: "a in (0, 1, NULL)"
        let list = vec![
            lit(ScalarValue::Int64(Some(0))),
            lit(ScalarValue::Int64(Some(1))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), None, None],
            col_a.clone()
        );

        // expression: "a not in (0, 1, NULL)"
        let list = vec![
            lit(ScalarValue::Int64(Some(0))),
            lit(ScalarValue::Int64(Some(1))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), None, None],
            col_a.clone()
        );

        Ok(())
    }

    #[test]
    fn in_list_float64() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Float64, true)]);
        let a = Float64Array::from(vec![Some(0.0), Some(0.2), None]);
        let col_a = col("a", &schema)?;
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a)])?;

        // expression: "a in (0.0, 0.2)"
        let list = vec![
            lit(ScalarValue::Float64(Some(0.0))),
            lit(ScalarValue::Float64(Some(0.1))),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), Some(false), None],
            col_a.clone()
        );

        // expression: "a not in (0.0, 0.2)"
        let list = vec![
            lit(ScalarValue::Float64(Some(0.0))),
            lit(ScalarValue::Float64(Some(0.1))),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), Some(true), None],
            col_a.clone()
        );

        // expression: "a in (0.0, 0.2, NULL)"
        let list = vec![
            lit(ScalarValue::Float64(Some(0.0))),
            lit(ScalarValue::Float64(Some(0.1))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &false,
            vec![Some(true), None, None],
            col_a.clone()
        );

        // expression: "a not in (0.0, 0.2, NULL)"
        let list = vec![
            lit(ScalarValue::Float64(Some(0.0))),
            lit(ScalarValue::Float64(Some(0.1))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(
            batch,
            list,
            &true,
            vec![Some(false), None, None],
            col_a.clone()
        );

        Ok(())
    }

    #[test]
    fn in_list_bool() -> Result<()> {
        let schema = Schema::new(vec![Field::new("a", DataType::Boolean, true)]);
        let a = BooleanArray::from(vec![Some(true), None]);
        let col_a = col("a", &schema)?;
        let batch = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a)])?;

        // expression: "a in (true)"
        let list = vec![lit(ScalarValue::Boolean(Some(true)))];
        in_list!(batch, list, &false, vec![Some(true), None], col_a.clone());

        // expression: "a not in (true)"
        let list = vec![lit(ScalarValue::Boolean(Some(true)))];
        in_list!(batch, list, &true, vec![Some(false), None], col_a.clone());

        // expression: "a in (true, NULL)"
        let list = vec![
            lit(ScalarValue::Boolean(Some(true))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(batch, list, &false, vec![Some(true), None], col_a.clone());

        // expression: "a not in (true, NULL)"
        let list = vec![
            lit(ScalarValue::Boolean(Some(true))),
            lit(ScalarValue::Utf8(None)),
        ];
        in_list!(batch, list, &true, vec![Some(false), None], col_a.clone());

        Ok(())
    }
}
