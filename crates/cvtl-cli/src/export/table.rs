// FILE: crates/cvtl-cli/src/export/table.rs
// VERSION: 0.1.0
// START_MODULE_CONTRACT
//   PURPOSE: Encode the same verified tick series as CSV and Parquet
//   SCOPE: Shared column schema, nullable cells and bounded row groups
//   DEPENDS: M-CVTL-CLI
//   LINKS: M-CVTL-CLI
//   ROLE: RUNTIME
//   MAP_MODE: EXPORTS
// END_MODULE_CONTRACT
// START_MODULE_MAP
//   Tables - write and sync both tables in a private staging directory
// END_MODULE_MAP

use std::{
    fmt::Display,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    sync::Arc,
};

use cvtl_sim::modes::BodyPort;
use cvtl_store::record::{ModeBodyChangeV1, ModeChangeOutcomeV1, ModeCompensationV1};
use parquet::{
    basic::Compression,
    column::writer::ColumnWriterImpl,
    data_type::{BoolType, ByteArray, ByteArrayType, DataType, DoubleType, Int64Type},
    errors::Result as ParquetResult,
    file::{
        properties::WriterProperties,
        writer::{SerializedColumnWriter, SerializedFileWriter},
    },
    schema::parser::parse_message_type,
};

use crate::replay::{ModeTimelineSample, TimelineSample};

pub(super) const CSV: &str = "ticks.csv";
pub(super) const PARQUET: &str = "ticks.parquet";
const ROW_GROUP_ROWS: usize = 256;

enum Column {
    Unsigned(&'static str, fn(&TimelineSample) -> u64),
    OptionalUnsigned(&'static str, fn(&TimelineSample) -> Option<u64>),
    Double(&'static str, fn(&TimelineSample) -> Option<f64>),
    Flag(&'static str, fn(&TimelineSample) -> Option<bool>),
    Text(&'static str, fn(&TimelineSample) -> Option<&str>),
}

const COLUMNS: &[Column] = &[
    Column::Unsigned("tick", |row| row.tick),
    Column::Unsigned("seq", |row| row.seq),
    Column::Unsigned("measured_ns", |row| row.measured_ns),
    Column::Double("stage1_k", |row| row.temperatures_k[0]),
    Column::Double("stage2_k", |row| row.temperatures_k[1]),
    Column::Double("tf_k", |row| row.temperatures_k[2]),
    Column::Double("jacket_k", |row| row.temperatures_k[3]),
    Column::Double("body_k", |row| row.body_k),
    Column::Double("inversion_delta_k", |row| row.inversion_delta_k),
    Column::Double("measured_a_w", |row| row.measured_w[0]),
    Column::Double("measured_b_w", |row| row.measured_w[1]),
    Column::Double("measured_total_w", |row| {
        row.measured_w[0]
            .filter(|watts| watts.is_finite())
            .zip(row.measured_w[1].filter(|watts| watts.is_finite()))
            .map(|(a, b)| a + b)
            .filter(|watts| watts.is_finite())
    }),
    Column::Double("requested_w", |row| row.requested_w),
    Column::Double("limited_w", |row| row.limited_w),
    Column::Double("target_a_w", |row| row.targets_w.map(|targets| targets[0])),
    Column::Double("target_b_w", |row| row.targets_w.map(|targets| targets[1])),
    Column::Double("bridge_flow_w", |row| row.bridge_flow_w),
    Column::Double("tracking_error_w", |row| row.tracking_error_w),
    Column::Double("changed_mass_kg", |row| {
        row.change
            .and_then(|change| change.mass_kg)
            .or_else(|| applied_change(row)?.request["mass_kg"].as_f64())
    }),
    Column::Double("changed_scale", |row| {
        row.change
            .and_then(|change| change.scale)
            .or_else(|| applied_change(row)?.request["scale"].as_f64())
    }),
    Column::Double("changed_bridge_w_per_k", |row| {
        row.change
            .and_then(|change| change.bridge_w_per_k)
            .or_else(|| {
                let request = &applied_change(row)?.request;
                request["g_w_per_k"]
                    .as_f64()
                    .or_else(|| request["bridge"]["conductance_w_per_k"].as_f64())
            })
    }),
    Column::Double("dh_event_j", |row| {
        row.change.and_then(|change| change.dh_event_j).or_else(|| {
            match applied_change(row)?.outcome {
                ModeChangeOutcomeV1::Applied { dh_event_j, .. } => dh_event_j,
                ModeChangeOutcomeV1::Rejected => None,
            }
        })
    }),
    Column::Double("temperature_before_change_k", |row| {
        row.change
            .map(|change| change.temperature_before_k)
            .or_else(|| match applied_change(row)?.outcome {
                ModeChangeOutcomeV1::Applied {
                    temperature_before_k,
                    ..
                } => Some(temperature_before_k),
                ModeChangeOutcomeV1::Rejected => None,
            })
    }),
    Column::Double("temperature_after_change_k", |row| {
        row.change
            .map(|change| change.temperature_after_k)
            .or_else(|| match applied_change(row)?.outcome {
                ModeChangeOutcomeV1::Applied {
                    temperature_after_k,
                    ..
                } => Some(temperature_after_k),
                ModeChangeOutcomeV1::Rejected => None,
            })
    }),
    Column::Flag("slew_limited", |row| {
        row.equivalence.as_ref().map(|_| row.slew_limited)
    }),
    Column::Flag("power_limited", |row| {
        row.equivalence.as_ref().map(|_| row.power_limited)
    }),
    Column::Flag("current_limited", |row| {
        row.equivalence.as_ref().map(|_| row.current_limited)
    }),
    Column::Text("equivalence", |row| row.equivalence.as_deref()),
];

const MODE_COLUMNS: &[Column] = &[
    Column::Text("mode", |row| Some(row.mode_v1.as_ref()?.kind)),
    Column::Double("hold_setpoint_k", |row| {
        row.mode_v1.as_ref()?.hold_setpoint_k
    }),
    Column::Text("body_port", |row| {
        Some(match row.mode_v1.as_ref()?.port? {
            BodyPort::Stage2 => "stage2",
            BodyPort::Tf => "tf",
        })
    }),
    Column::OptionalUnsigned("held_age_ns", |row| row.mode_v1.as_ref()?.held_age_ns),
    Column::Double("d_tf_k_per_s", |row| {
        match row.mode_v1.as_ref()?.compensation? {
            ModeCompensationV1::Ready { d_tf_k_per_s, .. } => Some(d_tf_k_per_s),
            ModeCompensationV1::Unavailable { .. } => None,
        }
    }),
    Column::Double("fixture_capacity_j_per_k", |row| {
        match row.mode_v1.as_ref()?.compensation? {
            ModeCompensationV1::Ready {
                fixture_capacity_j_per_k,
                ..
            } => Some(fixture_capacity_j_per_k),
            ModeCompensationV1::Unavailable { .. } => None,
        }
    }),
    Column::Double("fixture_parasitic_w", |row| {
        match row.mode_v1.as_ref()?.compensation? {
            ModeCompensationV1::Ready {
                fixture_parasitic_w,
                ..
            } => Some(fixture_parasitic_w),
            ModeCompensationV1::Unavailable { .. } => None,
        }
    }),
    Column::Text("changed_material", |row| {
        row.mode_v1
            .as_ref()
            .and_then(ModeTimelineSample::changed_material)
    }),
    Column::Text("changed_bridge_json", |row| {
        row.mode_v1.as_ref()?.changed_bridge_json.as_deref()
    }),
];

fn applied_change(row: &TimelineSample) -> Option<&ModeBodyChangeV1> {
    let change = row.mode_v1.as_ref()?.change.as_ref()?;
    matches!(change.outcome, ModeChangeOutcomeV1::Applied { .. }).then_some(change)
}

fn columns(operator_modes: bool) -> impl Iterator<Item = &'static Column> {
    COLUMNS
        .iter()
        .chain(if operator_modes { MODE_COLUMNS } else { &[] })
}

impl Column {
    fn name(&self) -> &'static str {
        match self {
            Self::Unsigned(name, _)
            | Self::OptionalUnsigned(name, _)
            | Self::Double(name, _)
            | Self::Flag(name, _)
            | Self::Text(name, _) => name,
        }
    }

    fn parquet_declaration(&self, schema: &mut String) {
        let (physical, annotation) = match self {
            Self::Unsigned(..) => ("REQUIRED INT64 ", " (INTEGER(64,false))"),
            Self::OptionalUnsigned(..) => ("OPTIONAL INT64 ", " (INTEGER(64,false))"),
            Self::Double(..) => ("OPTIONAL DOUBLE ", ""),
            Self::Flag(..) => ("OPTIONAL BOOLEAN ", ""),
            Self::Text(..) => ("OPTIONAL BINARY ", " (STRING)"),
        };
        schema.push_str(physical);
        schema.push_str(self.name());
        schema.push_str(annotation);
        schema.push_str(";\n");
    }

    fn write_csv(&self, output: &mut impl Write, row: &TimelineSample) -> io::Result<()> {
        match self {
            Self::Unsigned(_, read) => write!(output, "{}", read(row)),
            Self::OptionalUnsigned(_, read) => write_csv_value(output, read(row)),
            Self::Double(_, read) => write_csv_value(output, read(row)),
            Self::Flag(_, read) => write_csv_value(output, read(row)),
            Self::Text(_, read) => match read(row) {
                Some(text) if text.is_empty() || text.contains([',', '"', '\r', '\n']) => {
                    write!(output, "\"{}\"", text.replace('"', "\"\""))
                }
                Some(text) => output.write_all(text.as_bytes()),
                None => Ok(()),
            },
        }
    }

    fn write_parquet(
        &self,
        output: &mut SerializedColumnWriter<'_>,
        rows: &[TimelineSample],
    ) -> ParquetResult<()> {
        match self {
            Self::Unsigned(_, read) => {
                let values: Vec<_> = rows.iter().map(|row| parquet_unsigned(read(row))).collect();
                output
                    .typed::<Int64Type>()
                    .write_batch(&values, None, None)?;
                Ok(())
            }
            Self::OptionalUnsigned(_, read) => write_nullable(
                output.typed::<Int64Type>(),
                rows.iter().map(|row| read(row).map(parquet_unsigned)),
            ),
            Self::Double(_, read) => {
                write_nullable(output.typed::<DoubleType>(), rows.iter().map(read))
            }
            Self::Flag(_, read) => {
                write_nullable(output.typed::<BoolType>(), rows.iter().map(read))
            }
            Self::Text(_, read) => write_nullable(
                output.typed::<ByteArrayType>(),
                rows.iter().map(|row| read(row).map(ByteArray::from)),
            ),
        }
    }
}

fn parquet_unsigned(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

fn write_csv_value(output: &mut impl Write, value: Option<impl Display>) -> io::Result<()> {
    match value {
        Some(value) => write!(output, "{value}"),
        None => Ok(()),
    }
}

fn write_nullable<T: DataType>(
    output: &mut ColumnWriterImpl<T>,
    cells: impl ExactSizeIterator<Item = Option<T::T>>,
) -> ParquetResult<()> {
    let mut values = Vec::with_capacity(cells.len());
    let mut levels = Vec::with_capacity(cells.len());
    for cell in cells {
        levels.push(i16::from(cell.is_some()));
        values.extend(cell);
    }
    output.write_batch(&values, Some(&levels), None)?;
    Ok(())
}

pub(super) struct Tables {
    csv: BufWriter<File>,
    parquet: SerializedFileWriter<File>,
    batch: Vec<TimelineSample>,
    operator_modes: bool,
    rows: u64,
}

impl Tables {
    pub(super) fn open(dir: &Path, operator_modes: bool) -> Result<Self, String> {
        let mut csv = BufWriter::new(File::create_new(dir.join(CSV)).map_err(table_error)?);
        let header = columns(operator_modes)
            .map(Column::name)
            .collect::<Vec<_>>()
            .join(",");
        writeln!(csv, "{header}").map_err(table_error)?;

        let mut schema = String::from("message cvtl_tick {\n");
        for column in columns(operator_modes) {
            column.parquet_declaration(&mut schema);
        }
        schema.push('}');
        let schema = Arc::new(parse_message_type(&schema).map_err(table_error)?);
        let properties = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::UNCOMPRESSED)
                .set_dictionary_enabled(false)
                .build(),
        );
        let file = File::create_new(dir.join(PARQUET)).map_err(table_error)?;
        let parquet = SerializedFileWriter::new(file, schema, properties).map_err(table_error)?;
        Ok(Self {
            csv,
            parquet,
            batch: Vec::with_capacity(ROW_GROUP_ROWS),
            operator_modes,
            rows: 0,
        })
    }

    pub(super) fn push(&mut self, sample: TimelineSample) -> Result<(), String> {
        for (index, column) in columns(self.operator_modes).enumerate() {
            if index > 0 {
                self.csv.write_all(b",").map_err(table_error)?;
            }
            column
                .write_csv(&mut self.csv, &sample)
                .map_err(table_error)?;
        }
        self.csv.write_all(b"\n").map_err(table_error)?;
        self.batch.push(sample);
        if self.batch.len() == ROW_GROUP_ROWS {
            self.flush_group().map_err(table_error)?;
        }
        self.rows += 1;
        Ok(())
    }

    fn flush_group(&mut self) -> ParquetResult<()> {
        let mut group = self.parquet.next_row_group()?;
        for column in columns(self.operator_modes) {
            let mut writer = group
                .next_column()?
                .expect("schema and columns share one definition");
            column.write_parquet(&mut writer, &self.batch)?;
            writer.close()?;
        }
        group.close()?;
        self.batch.clear();
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<u64, String> {
        if !self.batch.is_empty() {
            self.flush_group().map_err(table_error)?;
        }
        self.csv.flush().map_err(table_error)?;
        self.csv.get_ref().sync_all().map_err(table_error)?;
        self.parquet
            .into_inner()
            .map_err(table_error)?
            .sync_all()
            .map_err(table_error)?;
        Ok(self.rows)
    }
}

fn table_error(error: impl Display) -> String {
    format!("Ошибка записи таблиц экспорта ({error})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::{
        file::reader::{FileReader, SerializedFileReader},
        record::{Field, RowAccessor},
    };
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "cvtl-export-table-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tables_preserve_unsigned_values_nulls_and_row_group_bounds() {
        let directory = TestDirectory::new();
        let mut tables = Tables::open(&directory.0, false).unwrap();
        let first = TimelineSample {
            tick: (1 << 53) + 1,
            seq: u64::MAX,
            measured_ns: 1 << 63,
            temperatures_k: [Some(-0.0), None, None, None],
            body_k: None,
            measured_w: [Some(2.0), Some(3.0)],
            requested_w: None,
            bridge_flow_w: None,
            tracking_error_w: None,
            inversion_delta_k: None,
            limited_w: None,
            targets_w: None,
            slew_limited: false,
            power_limited: true,
            current_limited: false,
            equivalence: Some(String::new()),
            change: None,
            mode_v1: None,
        };
        tables.push(first.clone()).unwrap();
        let mut missing = first;
        missing.equivalence = None;
        missing.measured_w = [Some(f64::MAX), Some(f64::MAX)];
        for _ in 0..ROW_GROUP_ROWS {
            tables.push(missing.clone()).unwrap();
        }
        assert_eq!(tables.finish().unwrap(), 257);

        let csv = fs::read_to_string(directory.0.join(CSV)).unwrap();
        let mut lines = csv.lines();
        let names: Vec<_> = lines.next().unwrap().split(',').collect();
        assert_eq!(names, COLUMNS.iter().map(Column::name).collect::<Vec<_>>());
        let index = |name| names.iter().position(|column| *column == name).unwrap();
        let present: Vec<_> = lines.next().unwrap().split(',').collect();
        let absent: Vec<_> = lines.next().unwrap().split(',').collect();
        assert_eq!(present[0], "9007199254740993");
        assert_eq!(present[1], "18446744073709551615");
        assert_eq!(present[2], "9223372036854775808");
        assert_eq!(present[index("stage1_k")], "-0");
        assert_eq!(present[index("measured_total_w")], "5");
        assert_eq!(present[index("power_limited")], "true");
        assert_eq!(present[index("equivalence")], "\"\"");
        assert_eq!(absent[index("measured_total_w")], "");
        assert_eq!(absent[index("power_limited")], "");
        assert_eq!(absent[index("equivalence")], "");
        assert_eq!(lines.count(), 255);

        let reader =
            SerializedFileReader::new(File::open(directory.0.join(PARQUET)).unwrap()).unwrap();
        let groups: Vec<_> = reader
            .metadata()
            .row_groups()
            .iter()
            .map(|group| group.num_rows())
            .collect();
        assert_eq!(groups, [256, 1]);
        let mut rows = reader.get_row_iter(None).unwrap();
        let present = rows.next().unwrap().unwrap();
        let absent = rows.next().unwrap().unwrap();
        let fields: Vec<_> = present.get_column_iter().collect();
        assert_eq!(
            fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            names
        );
        assert_eq!(present.get_ulong(0).unwrap(), (1 << 53) + 1);
        assert_eq!(present.get_ulong(1).unwrap(), u64::MAX);
        assert_eq!(present.get_ulong(2).unwrap(), 1 << 63);
        assert_eq!(
            present.get_double(index("stage1_k")).unwrap().to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(present.get_double(index("measured_total_w")).unwrap(), 5.0);
        assert!(present.get_bool(index("power_limited")).unwrap());
        assert_eq!(present.get_string(index("equivalence")).unwrap(), "");
        let fields: Vec<_> = absent.get_column_iter().map(|(_, field)| field).collect();
        for name in [
            "stage2_k",
            "measured_total_w",
            "power_limited",
            "equivalence",
        ] {
            assert!(matches!(fields[index(name)], Field::Null), "{name}");
        }
        assert_eq!(rows.collect::<ParquetResult<Vec<_>>>().unwrap().len(), 255);
    }

    #[test]
    fn empty_export_has_a_csv_header_and_a_parquet_schema() {
        let directory = TestDirectory::new();
        assert_eq!(
            Tables::open(&directory.0, false).unwrap().finish().unwrap(),
            0
        );
        let csv = fs::read_to_string(directory.0.join(CSV)).unwrap();
        assert_eq!(csv.lines().count(), 1);
        assert_eq!(csv.trim_end().split(',').count(), COLUMNS.len());
        let reader =
            SerializedFileReader::new(File::open(directory.0.join(PARQUET)).unwrap()).unwrap();
        assert_eq!(
            reader
                .metadata()
                .file_metadata()
                .schema_descr()
                .num_columns(),
            COLUMNS.len()
        );
        assert_eq!(reader.metadata().num_row_groups(), 0);
        assert!(reader.get_row_iter(None).unwrap().next().is_none());
    }
}
