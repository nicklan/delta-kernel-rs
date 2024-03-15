use crate::engine_data::{EngineData, EngineList, EngineMap, GetData};
use crate::schema::{DataType, PrimitiveType, Schema, SchemaRef, StructField};
use crate::{DataVisitor, DeltaResult, Error};

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, Int64Type};
use arrow_array::{Array, GenericListArray, MapArray, RecordBatch, StructArray};
use arrow_schema::{ArrowError, DataType as ArrowDataType, Schema as ArrowSchema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tracing::{debug, warn};
use url::Url;

use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

/// SimpleData holds a RecordBatch, implements `EngineData` so the kernel can extract from it.
pub struct SimpleData {
    data: RecordBatch,
}

impl SimpleData {
    /// Create a new `SimpleData` from a `RecordBatch`
    pub fn new(data: RecordBatch) -> Self {
        SimpleData { data }
    }

    /// Utility constructor to get a `Box<SimpleData>` out of a `Box<dyn EngineData>`
    pub fn try_from_engine_data(engine_data: Box<dyn EngineData>) -> DeltaResult<Box<Self>> {
        engine_data
            .into_any()
            .downcast::<SimpleData>()
            .map_err(|_| Error::engine_data_type("SimpleData"))
    }

    /// Get a reference to the `RecordBatch` this `SimpleData` is wrapping
    pub fn record_batch(&self) -> &RecordBatch {
        &self.data
    }
}

impl EngineData for SimpleData {
    fn extract(&self, schema: SchemaRef, visitor: &mut dyn DataVisitor) -> DeltaResult<()> {
        let mut col_array = vec![];
        self.extract_columns(&mut col_array, &schema)?;
        visitor.visit(self.length(), &col_array)
    }

    fn length(&self) -> usize {
        self.data.num_rows()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl From<RecordBatch> for SimpleData {
    fn from(value: RecordBatch) -> Self {
        SimpleData::new(value)
    }
}

impl From<SimpleData> for RecordBatch {
    fn from(value: SimpleData) -> Self {
        value.data
    }
}

impl From<Box<SimpleData>> for RecordBatch {
    fn from(value: Box<SimpleData>) -> Self {
        value.data
    }
}

/// This is a trait that allows us to query something by column name and get out an Arrow
/// `Array`. Both `RecordBatch` and `StructArray` can do this. By having our `extract_*` functions
/// just take anything that implements this trait we can use the same function to drill into
/// either. This is useful because when we're recursing into data we start with a RecordBatch, but
/// if we encounter a Struct column, it will be a `StructArray`.
trait ProvidesColumnByName {
    fn column_by_name(&self, name: &str) -> Option<&Arc<dyn Array>>;
}

impl ProvidesColumnByName for RecordBatch {
    fn column_by_name(&self, name: &str) -> Option<&Arc<dyn Array>> {
        self.column_by_name(name)
    }
}

impl ProvidesColumnByName for StructArray {
    fn column_by_name(&self, name: &str) -> Option<&Arc<dyn Array>> {
        self.column_by_name(name)
    }
}

impl EngineList for GenericListArray<i32> {
    fn len(&self, row_index: usize) -> usize {
        self.value(row_index).len()
    }

    fn get(&self, row_index: usize, index: usize) -> String {
        let arry = self.value(row_index);
        let sarry = arry.as_string::<i32>();
        sarry.value(index).to_string()
    }

    fn materialize(&self, row_index: usize) -> Vec<String> {
        let mut result = vec![];
        for i in 0..EngineList::len(self, row_index) {
            result.push(self.get(row_index, i));
        }
        result
    }
}

impl EngineMap for MapArray {
    fn get<'a>(&'a self, row_index: usize, key: &str) -> Option<&'a str> {
        let offsets = self.offsets();
        let start_offset = offsets[row_index] as usize;
        let count = offsets[row_index + 1] as usize - start_offset;
        let keys = self.keys().as_string::<i32>();
        for (idx, map_key) in keys.iter().enumerate().skip(start_offset).take(count) {
            if let Some(map_key) = map_key {
                if key == map_key {
                    // found the item
                    let vals = self.values().as_string::<i32>();
                    return Some(vals.value(idx));
                }
            }
        }
        None
    }

    fn materialize(&self, row_index: usize) -> HashMap<String, Option<String>> {
        let mut ret = HashMap::new();
        let map_val = self.value(row_index);
        let keys = map_val.column(0).as_string::<i32>();
        let values = map_val.column(1).as_string::<i32>();
        for (key, value) in keys.iter().zip(values.iter()) {
            if let Some(key) = key {
                ret.insert(key.into(), value.map(|v| v.into()));
            }
        }
        ret
    }
}

impl SimpleData {
    pub fn try_create_from_json(schema: SchemaRef, location: Url) -> DeltaResult<Self> {
        let arrow_schema: ArrowSchema = (&*schema).try_into()?;
        debug!("Reading {:#?} with schema: {:#?}", location, arrow_schema);
        // todo: Check scheme of url
        let file = File::open(
            location
                .to_file_path()
                .map_err(|_| Error::generic("can only read local files"))?,
        )?;
        let mut json =
            arrow_json::ReaderBuilder::new(Arc::new(arrow_schema)).build(BufReader::new(file))?;
        let data = json
            .next()
            .ok_or(Error::generic("No data found reading json file"))?;
        Ok(SimpleData::new(data?))
    }

    // TODO needs to apply the schema to the parquet read
    pub fn try_create_from_parquet(_schema: SchemaRef, location: Url) -> DeltaResult<Self> {
        let file = File::open(
            location
                .to_file_path()
                .map_err(|_| Error::generic("can only read local files"))?,
        )?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let mut reader = builder.build()?;
        let data = reader
            .next()
            .ok_or(Error::generic("No data found reading parquet file"))?;
        Ok(SimpleData::new(data?))
    }

    /// Extracts an exploded view (all leaf values), in schema order of that data contained
    /// within. `out_col_array` is filled with [`GetData`] items that can be used to get at the
    /// actual primitive types.
    ///
    /// # Arguments
    ///
    /// * `out_col_array` - the vec that leaf values will be pushed onto. it is passed as an arg to
    /// make the recursion below easier. if we returned a [`Vec`] we would have to `extend` it each
    /// time we encountered a struct and made the recursive call.
    /// * `schema` - the schema to extract getters for
    pub fn extract_columns<'a>(
        &'a self,
        out_col_array: &mut Vec<&dyn GetData<'a>>,
        schema: &Schema,
    ) -> DeltaResult<()> {
        debug!("Extracting column getters for {:#?}", schema);
        SimpleData::extract_columns_from_array(out_col_array, schema, Some(&self.data))
    }

    fn extract_columns_from_array<'a>(
        out_col_array: &mut Vec<&dyn GetData<'a>>,
        schema: &Schema,
        array: Option<&'a dyn ProvidesColumnByName>,
    ) -> DeltaResult<()> {
        for field in schema.fields() {
            let col = array
                .and_then(|a| a.column_by_name(&field.name))
                .filter(|a| *a.data_type() != ArrowDataType::Null);
            // Note: if col is None we have either:
            //   a) encountered a column that is all nulls or,
            //   b) recursed into a optional struct that was null. In this case, array.is_none() is
            //      true and we don't need to check field nullability, because we assume all fields
            //      of a nullable struct can be null
            // So below if the field is allowed to be null, OR array.is_none() we push that,
            // otherwise we error out.
            if let Some(col) = col {
                Self::extract_column(out_col_array, field, col)?;
            } else if array.is_none() || field.is_nullable() {
                if let DataType::Struct(inner_struct) = field.data_type() {
                    Self::extract_columns_from_array(out_col_array, inner_struct.as_ref(), None)?;
                } else {
                    debug!("Pushing a null field for {}", field.name);
                    out_col_array.push(&());
                }
            } else {
                return Err(Error::MissingData(format!(
                    "Found required field {}, but it's null",
                    field.name
                )));
            }
        }
        Ok(())
    }

    fn extract_column<'a>(
        out_col_array: &mut Vec<&dyn GetData<'a>>,
        field: &StructField,
        col: &'a dyn Array,
    ) -> DeltaResult<()> {
        match (col.data_type(), &field.data_type) {
            (&ArrowDataType::Struct(_), DataType::Struct(fields)) => {
                // both structs, so recurse into col
                let struct_array = col.as_struct();
                SimpleData::extract_columns_from_array(out_col_array, fields, Some(struct_array))?;
            }
            (&ArrowDataType::Boolean, &DataType::Primitive(PrimitiveType::Boolean)) => {
                debug!("Pushing boolean array for {}", field.name);
                out_col_array.push(col.as_boolean());
            }
            (&ArrowDataType::Utf8, &DataType::Primitive(PrimitiveType::String)) => {
                debug!("Pushing string array for {}", field.name);
                out_col_array.push(col.as_string::<i32>());
            }
            (&ArrowDataType::Int32, &DataType::Primitive(PrimitiveType::Integer)) => {
                debug!("Pushing int32 array for {}", field.name);
                out_col_array.push(col.as_primitive::<Int32Type>());
            }
            (&ArrowDataType::Int64, &DataType::Primitive(PrimitiveType::Long)) => {
                debug!("Pushing int64 array for {}", field.name);
                out_col_array.push(col.as_primitive::<Int64Type>());
            }
            (ArrowDataType::List(_arrow_field), DataType::Array(_array_type)) => {
                // TODO(nick): validate the element types match
                debug!("Pushing list for {}", field.name);
                out_col_array.push(col.as_list());
            }
            (&ArrowDataType::Map(_, _), &DataType::Map(_)) => {
                debug!("Pushing map for {}", field.name);
                out_col_array.push(col.as_map());
            }
            (arrow_data_type, data_type) => {
                warn!(
                    "Can't extract {}. Arrow Type: {arrow_data_type}\n Kernel Type: {data_type}",
                    field.name
                );
                return Err(get_error_for_types(data_type, arrow_data_type, &field.name));
            }
        }
        Ok(())
    }
}

fn get_error_for_types(
    data_type: &DataType,
    arrow_data_type: &ArrowDataType,
    field_name: &str,
) -> Error {
    let expected_type: Result<ArrowDataType, ArrowError> = data_type.try_into();
    match expected_type {
        Ok(expected_type) => {
            if expected_type == *arrow_data_type {
                Error::UnexpectedColumnType(format!(
                    "On {field_name}: Don't know how to extract something of type {data_type}",
                ))
            } else {
                Error::UnexpectedColumnType(format!(
                    "Type mismatch on {field_name}: expected {data_type}, got {arrow_data_type}",
                ))
            }
        }
        Err(e) => Error::UnexpectedColumnType(format!(
            "On {field_name}: Unsupported data type {data_type}: {e}",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema as ArrowSchema};

    use crate::actions::{Metadata, Protocol};
    use crate::DeltaResult;
    use crate::{
        actions::get_log_schema,
        simple_client::{data::SimpleData, SimpleClient},
        EngineData, EngineInterface,
    };

    fn string_array_to_engine_data(string_array: StringArray) -> Box<dyn EngineData> {
        let string_field = Arc::new(Field::new("a", DataType::Utf8, true));
        let schema = Arc::new(ArrowSchema::new(vec![string_field]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(string_array)])
            .expect("Can't convert to record batch");
        Box::new(SimpleData::new(batch))
    }

    #[test]
    fn test_md_extract() -> DeltaResult<()> {
        let client = SimpleClient::new();
        let handler = client.get_json_handler();
        let json_strings: StringArray = vec![
            r#"{"metaData":{"id":"aff5cb91-8cd9-4195-aef9-446908507302","format":{"provider":"parquet","options":{}},"schemaString":"{\"type\":\"struct\",\"fields\":[{\"name\":\"c1\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"c2\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}},{\"name\":\"c3\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}}]}","partitionColumns":["c1","c2"],"configuration":{},"createdTime":1670892997849}}"#,
        ]
        .into();
        let output_schema = Arc::new(get_log_schema().clone());
        let parsed = handler
            .parse_json(string_array_to_engine_data(json_strings), output_schema)
            .unwrap();
        let metadata = Metadata::try_new_from_data(parsed.as_ref())?.unwrap();
        assert_eq!(metadata.id, "aff5cb91-8cd9-4195-aef9-446908507302");
        assert_eq!(metadata.created_time, Some(1670892997849));
        assert_eq!(metadata.partition_columns, vec!("c1", "c2"));
        Ok(())
    }

    #[test]
    fn test_nullable_struct() -> DeltaResult<()> {
        let client = SimpleClient::new();
        let handler = client.get_json_handler();
        let json_strings: StringArray = vec![
            r#"{"metaData":{"id":"aff5cb91-8cd9-4195-aef9-446908507302","format":{"provider":"parquet","options":{}},"schemaString":"{\"type\":\"struct\",\"fields\":[{\"name\":\"c1\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}},{\"name\":\"c2\",\"type\":\"string\",\"nullable\":true,\"metadata\":{}},{\"name\":\"c3\",\"type\":\"integer\",\"nullable\":true,\"metadata\":{}}]}","partitionColumns":["c1","c2"],"configuration":{},"createdTime":1670892997849}}"#,
        ]
        .into();
        let output_schema = get_log_schema().project_as_schema(&["metaData"])?;
        let parsed = handler
            .parse_json(string_array_to_engine_data(json_strings), output_schema)
            .unwrap();
        let protocol = Protocol::try_new_from_data(parsed.as_ref())?;
        assert!(protocol.is_none());
        Ok(())
    }
}
